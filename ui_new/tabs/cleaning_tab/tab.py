from __future__ import annotations

import os
from typing import List, Optional

from PyQt6.QtCore import QEvent, Qt
from PyQt6.QtGui import QKeySequence, QShortcut
from PyQt6.QtWidgets import (
    QApplication,
    QCheckBox,
    QComboBox,
    QHBoxLayout,
    QLabel,
    QMessageBox,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from ui_new.tabs.cleaning_tab.fast_clean import FastCleanPanel, FastCleanProcessor
from ui_new.tabs.cleaning_tab.view import DrawingCanvasView


def _load_images_from_dir(dir_path: str) -> List[str]:
    if not dir_path or not os.path.isdir(dir_path):
        return []
    entries = []
    for fn in os.listdir(dir_path):
        p = os.path.join(dir_path, fn)
        if os.path.isfile(p) and os.path.splitext(p)[1].lower() in (".png", ".jpg", ".jpeg"):
            entries.append(p)
    return entries


class CleaningTab(QWidget):
    """
    Полноценная вкладка клининга:
     • Лента изображений из project.cleaned_dir
     • Прозрачные оверлеи в исходном размере для рисования
     • Сохранение: альфа-композит в PNG в ту же папку
     • Откат текущей страницы к оригиналу из project.src_dir
    """

    def __init__(
        self,
        project,
        parent: Optional[QWidget] = None,
        bubbles_model=None,
        source_images=None,
        overlays_model=None,
        text_detection_model=None,
        user_config=None,
        ai_device=None,
    ):
        super().__init__(parent)
        self.project = project
        self.text_detection_model = text_detection_model
        self.ai_device = ai_device
        # ensure_saved() и load() уже вызваны в qt_runner.py:heavy_init()
        # Дублирующий вызов удалён для ускорения запуска

        images = source_images if source_images is not None else _load_images_from_dir(getattr(self.project, "src_dir", ""))

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 7, 0, 0)  # left, top, right, bottom
        layout.setSpacing(7)  # убираем spacing между виджетами
        bar = QHBoxLayout()
        self.model = bubbles_model
        self.overlays_model = overlays_model

        # Базовая строка хоткеев для вкладки (устанавливаем ДО загрузки инструментов!)
        self._base_hotkeys = "Ctrl+Z — откат • Ctrl+Shift+Z — повтор • Ctrl+S — сохранить • Ctrl± — зум • T — пузыри"

        # >>> СБОРКА ПАНЕЛИ ИНСТРУМЕНТОВ (сначала очистка/видимость/комбо/динамический UI)
        self.view = DrawingCanvasView(
            self.project,
            images,
            parent=self,
            bubbles_model=self.model,
            overlays_model=self.overlays_model,
            text_detection_model=text_detection_model,
            user_config=user_config,
        )
        self.view.ai_device = ai_device
        # Пока панель быстрого клина скрыта — не показываем результат детектора
        self.view.set_textdetector_visibility(False)
        self._tool_shortcuts: list[QShortcut] = []
        # ловим мышь с viewport() (чтобы CanvasView получил, если инструмент не обработал)
        self.view.viewport().installEventFilter(self)
        # ловим клавиатуру на самой вкладке (и дочерних)
        self.installEventFilter(self)
        self._fast_clean_processor = FastCleanProcessor(self.view)
        self._build_tools_panel(bar)
        self._init_fast_clean_panel()

        bar.addStretch(1)

        # Кнопки действий справа
        btn_save = QPushButton("Сохранить слои")
        btn_undo = QPushButton("Отменить (Ctrl+Z)")
        btn_redo = QPushButton("Повторить (Ctrl+Shift+Z)")
        bar.addWidget(btn_save)
        bar.addWidget(btn_undo)
        bar.addWidget(btn_redo)

        layout.addLayout(bar)
        layout.addWidget(self.view)

        # Используем _hotkeysLabel из CanvasView если доступен
        if hasattr(self.view, "_hotkeysLabel"):
            self._update_hotkeys_label()
        else:
            # Fallback если нет _hotkeysLabel
            hint = QLabel(self._base_hotkeys)
            hint.setStyleSheet("color: gray;")
            layout.addWidget(hint)

        btn_save.clicked.connect(self._on_save_all)
        btn_undo.clicked.connect(self.view.undo_current_page)
        btn_redo.clicked.connect(self.view.redo_current_page)
        app = QApplication.instance()  # <<<
        if app:
            app.aboutToQuit.connect(self._on_app_about_to_quit)

    def deactivate_active_tool(self) -> None:  # <<<
        """Снимает активный инструмент и останавливает взаимодействие с CanvasView."""
        # 1) Снять инструмент
        tool = getattr(self, "_active_tool", None)
        if tool:
            try:
                tool.deactivate()
            except Exception:
                pass
            self._active_tool = None

        # 2) Остановить текущие операции рисования
        if hasattr(self, "view") and self.view:
            try:
                self.view.finish_interaction()
            except Exception:
                pass
        # 3) Снять шорткаты инструмента
        try:
            self._clear_tool_shortcuts()
        except Exception:
            pass

    def _on_app_about_to_quit(self) -> None:  # <<<
        """Гарантированная зачистка перед закрытием приложения."""
        self.deactivate_active_tool()
        if hasattr(self, "view") and self.view:
            try:
                self.view.teardown_shortcuts()
            except Exception:
                pass

    def _on_save_all(self):
        self.view.save_all_to_cleaned_dir()
        QMessageBox.information(self, "Готово", "Слои сохранены в папку clean_layers.")

    def _activate_tool_by_id(self, tool_id: str):
        # деактивировать предыдущий
        self.deactivate_active_tool()
        # очистить динамическую панель
        while self._tool_ui_layout.count():
            item = self._tool_ui_layout.takeAt(0)
            w = item.widget()
            if w is not None:
                w.deleteLater()
        # создать новый инструмент
        ToolClass = self._tool_classes.get(tool_id)
        if not ToolClass:
            self._active_tool = None
            self._update_hotkeys_label()  # обновить без инструмента
            return
        self._active_tool = ToolClass()
        self._active_tool.activate(self.view)
        self._bind_tool_shortcuts()
        # пусть инструмент добавит свой UI
        try:
            self._active_tool.build_ui(self._tool_ui_layout)
        except Exception:
            pass
        # обновить строку хоткеев с учётом активного инструмента
        self._update_hotkeys_label()
        # синхронизируем id на стороне view (на будущее)
        self.view.set_tool_by_id(tool_id)

    def _on_tool_combo_changed(self, index: int):
        tid = self._tool_combo.itemData(index)
        if tid:
            self._activate_tool_by_id(tid)

    def _update_hotkeys_label(self) -> None:
        """Обновляет _hotkeysLabel с учётом базовых хоткеев и активного инструмента."""
        if not hasattr(self.view, "_hotkeysLabel"):
            return

        # Собираем строку: базовые хоткеи + хоткеи инструмента
        parts = [self._base_hotkeys]
        tool = getattr(self, "_active_tool", None)
        if tool:
            try:
                tool_hint = tool.hotkeys_hint()
                if tool_hint:
                    parts.append(tool_hint)
            except Exception:
                pass

        # Объединяем через разделитель
        full_hint = " • ".join(parts)
        self.view._hotkeysLabel.setText(full_hint)

    def _load_tools(self):
        """Загружает все инструменты из ui_new.tools и подготавливает комбобокс."""
        from ui_new.tools import load_all_tools

        self._tool_classes = load_all_tools()  # {tool_id: ToolClass}
        # Порядок в комбобоксе: отсортируем по title
        items = sorted([(cls.title, tid) for tid, cls in self._tool_classes.items()], key=lambda t: t[0].lower())
        self._tool_combo.clear()
        for title, tid in items:
            self._tool_combo.addItem(title, userData=tid)
        # автоселект первого
        if items:
            self._tool_combo.setCurrentIndex(0)
            self._activate_tool_by_id(items[0][1])

    def _build_tools_panel(self, bar_layout):
        from PyQt6.QtWidgets import QHBoxLayout, QLabel, QCheckBox, QComboBox, QPushButton, QWidget

        # Очистить текущий слой
        clear_btn = QPushButton("Очистить слой", self)
        clear_btn.clicked.connect(self._on_clear_current)
        bar_layout.addWidget(clear_btn)

        # Показать/скрыть слои
        vis_chk = QCheckBox("Показать слой", self)
        vis_chk.setChecked(True)
        vis_chk.toggled.connect(self._on_toggle_overlay_visible)
        bar_layout.addWidget(vis_chk)

        # Быстрый клин: плавающая панель, вызываем кнопкой
        quick_btn = QPushButton("Быстрый клин", self)
        quick_btn.setCheckable(True)
        quick_btn.setChecked(False)
        quick_btn.clicked.connect(self._on_toggle_fast_clean_panel)
        bar_layout.addWidget(quick_btn)

        # Выпадающий список инструментов
        self._tool_combo = QComboBox(self)
        self._tool_combo.currentIndexChanged.connect(self._on_tool_combo_changed)
        bar_layout.addWidget(QLabel("Инструмент:", self))
        bar_layout.addWidget(self._tool_combo)

        # ДИНАМИЧЕСКАЯ панель под элементы UI выбранного инструмента
        tool_ui_host = QWidget(self)
        self._tool_ui_layout = QHBoxLayout(tool_ui_host)
        self._tool_ui_layout.setContentsMargins(0, 0, 0, 0)
        self._tool_ui_layout.setSpacing(6)
        bar_layout.addWidget(tool_ui_host)

        # сохранить ссылки, если ещё не были
        self._w_vis_chk = vis_chk
        self._quick_btn = quick_btn

        # загрузить и отобразить инструменты
        self._load_tools()

    def _init_fast_clean_panel(self) -> None:
        self._fast_panel_guard = False
        self._fast_clean_panel = FastCleanPanel(self)
        # немного стартовой ширины
        self._fast_clean_panel.setFixedWidth(420)
        pos = self._fast_clean_panel.pos()
        self._fast_clean_panel.move(pos.x(), pos.y() + 100)
        self._fast_clean_panel.visibilityChanged.connect(self._on_fast_panel_visibility_changed)
        self._fast_clean_panel.maskVisibilityChanged.connect(self._on_fast_panel_mask_toggled)
        self._fast_clean_panel.linesVisibilityChanged.connect(self._on_fast_panel_lines_toggled)
        self._fast_clean_panel.blocksVisibilityChanged.connect(self._on_fast_panel_blocks_toggled)
        self._fast_clean_panel.applyRequested.connect(self._on_fast_panel_apply_requested)
        self._fast_clean_panel.applyAllRequested.connect(self._on_fast_panel_apply_all_requested)
        self._fast_clean_panel.closed.connect(self._on_fast_panel_closed)
        self._fast_clean_panel.uniformityToleranceChanged.connect(self._on_fast_panel_uniformity_changed)
        self._fast_clean_panel.hide()
        # инициализируем UI текущим допуском
        try:
            self._fast_clean_panel.set_uniformity_tolerance(self._fast_clean_processor.uniformity_tolerance())
        except Exception:
            pass
        self._attach_textdetector_model()

    def _on_opacity_changed(self, value: int):
        self.view.set_brush_opacity(value)

    def _on_toggle_overlay_visible(self, checked: bool):
        self.view.set_overlay_visible(checked)

    def _on_toggle_fast_clean_panel(self, checked: bool):
        if getattr(self, "_fast_panel_guard", False):
            return
        self._fast_clean_panel.set_panel_visible(checked)
        if checked:
            self._sync_fast_panel_position()

    def _on_fast_panel_visibility_changed(self, visible: bool):
        # синхронизируем кнопку без рекурсии
        self._fast_panel_guard = True
        try:
            if getattr(self, "_quick_btn", None):
                self._quick_btn.setChecked(bool(visible))
        finally:
            self._fast_panel_guard = False

        if visible:
            self._sync_fast_panel_position()
            self._fast_clean_panel.raise_()
            self.view.set_textdetector_visibility(
                True,
                show_mask=self._fast_clean_panel.mask_checked(),
                show_lines=self._fast_clean_panel.lines_checked(),
                show_blocks=self._fast_clean_panel.blocks_checked(),
            )
        else:
            self.view.set_textdetector_visibility(False)

    def _on_fast_panel_mask_toggled(self, checked: bool):
        if self._fast_clean_panel.isVisible():
            self.view.set_textdetector_mask_visible(checked)

    def _on_fast_panel_lines_toggled(self, checked: bool):
        if self._fast_clean_panel.isVisible():
            self.view.set_textdetector_lines_visible(checked)

    def _on_fast_panel_blocks_toggled(self, checked: bool):
        if self._fast_clean_panel.isVisible():
            self.view.set_textdetector_blocks_visible(checked)

    def _on_fast_panel_apply_requested(self, source: str):
        proc = getattr(self, "_fast_clean_processor", None)
        if proc is None:
            return
        ok, msg = proc.apply_current_page(source)
        try:
            if ok:
                QMessageBox.information(self, "Быстрый клин", msg or "Готово")
            else:
                QMessageBox.warning(self, "Быстрый клин", msg or "Не удалось выполнить замазку")
        except Exception:
            pass

    def _on_fast_panel_apply_all_requested(self, source: str):
        proc = getattr(self, "_fast_clean_processor", None)
        if proc is None:
            return
        ok, msg = proc.apply_all(source)
        try:
            if ok:
                QMessageBox.information(self, "Быстрый клин", msg or "Готово")
            else:
                QMessageBox.warning(self, "Быстрый клин", msg or "Не удалось выполнить замазку")
        except Exception:
            pass

    def _on_fast_panel_closed(self):
        # Закрытие крестиком синхронизирует кнопку
        self._on_fast_panel_visibility_changed(False)

    def _on_fast_panel_uniformity_changed(self, value: float):
        proc = getattr(self, "_fast_clean_processor", None)
        if proc is None:
            return
        try:
            proc.set_uniformity_tolerance(value)
        except Exception:
            pass

    # --- статус наличия детекции ---
    def _attach_textdetector_model(self) -> None:
        model = getattr(self, "text_detection_model", None)
        if model is None:
            self._update_fast_panel_detector_hint()
            return
        try:
            model.resultChanged.connect(lambda _idx: self._update_fast_panel_detector_hint())
            model.cleared.connect(lambda _idx: self._update_fast_panel_detector_hint())
            model.reset.connect(self._update_fast_panel_detector_hint)
        except Exception:
            pass
        self._update_fast_panel_detector_hint()

    def _has_textdet_results(self) -> bool:
        model = getattr(self, "text_detection_model", None)
        if model is None:
            return False
        try:
            data = model.as_dict()
            return bool(data)
        except Exception:
            return False

    def _update_fast_panel_detector_hint(self) -> None:
        panel = getattr(self, "_fast_clean_panel", None)
        if panel is None:
            return
        empty = not self._has_textdet_results()
        try:
            panel.set_textdetector_empty(empty)
        except Exception:
            pass

    def _on_clear_current(self):
        self.view.clear_current_overlay()

    def _sync_fast_panel_position(self) -> None:
        if not hasattr(self, "view") or self.view is None:
            return
        vg = self.view.geometry()
        margin = 12
        x = vg.left() + margin
        y = vg.top() + margin
        self._fast_clean_panel.move(x, y)
        self._fast_clean_panel.raise_()

    def resizeEvent(self, event):
        super().resizeEvent(event)
        if getattr(self, "_fast_clean_panel", None):
            self._sync_fast_panel_position()

    # --- Шорткаты текущего инструмента ---
    def _clear_tool_shortcuts(self) -> None:
        for sc in getattr(self, "_tool_shortcuts", []):
            try:
                sc.activated.disconnect()
            except Exception:
                pass
            sc.setParent(None)
            sc.deleteLater()
        self._tool_shortcuts = []

    def _bind_tool_shortcuts(self) -> None:
        """Создаёт QShortcut'ы согласно requested_shortcuts() активного инструмента."""
        self._clear_tool_shortcuts()
        tool = getattr(self, "_active_tool", None)
        if not tool:
            return
        try:
            pairs = list(tool.requested_shortcuts() or [])
        except Exception:
            pairs = []

        for seq, cb in pairs:
            try:
                qseq = QKeySequence(seq) if not isinstance(seq, QKeySequence) else seq
                sc = QShortcut(qseq, self)  # контекст на вкладке
                sc.setContext(Qt.ShortcutContext.WidgetWithChildrenShortcut)
                # заворачиваем колбэк в try, чтобы не ронять UI
                def _wrap(handler=cb):
                    try:
                        handler()
                    except Exception:
                        pass

                sc.activated.connect(_wrap)
                self._tool_shortcuts.append(sc)
            except Exception:
                pass

    def _is_tool_shortcut_event(self, event) -> bool:
        """Грубая проверка: совпадает ли KeyPress/ShortcutOverride с одним из активных QShortcut."""
        try:
            key = event.key()
            mods = int(event.modifiers())
        except Exception:
            return False
        for sc in self._tool_shortcuts:
            for seq in sc.key().toString().split(", "):  # поддержка множественных комбинаций
                if not seq:
                    continue
                # Сравним через QKeySequence: создаём из текущего события
                ev_seq = QKeySequence(mods | key)
                if ev_seq.matches(QKeySequence(seq)) == QKeySequence.SequenceMatch.ExactMatch:
                    return True
        return False

    def eventFilter(self, obj, event):
        et = event.type()

        # --- КОЛЕСО МЫШИ: отдать активному инструменту (например, для Shift+Wheel = размер кисти)
        if obj is self.view.viewport() and et == QEvent.Type.Wheel:
            tool = getattr(self, "_active_tool", None)
            if tool and hasattr(tool, "on_wheel_event"):
                try:
                    modifiers = getattr(event, "modifiers", lambda: Qt.KeyboardModifier.NoModifier)()
                    steps = 0

                    angle_delta_fn = getattr(event, "angleDelta", None)
                    if callable(angle_delta_fn):
                        ad = event.angleDelta()
                        try:
                            dy = ad.y()  # вертикальная прокрутка
                        except Exception:
                            dy = 0
                        if dy != 0:
                            # стандартный тик = 120, но на hi-res может быть меньше
                            steps = dy // 120
                            if steps == 0:
                                steps = 1 if dy > 0 else -1

                    handled = bool(tool.on_wheel_event(int(steps), modifiers))
                    if handled:
                        try:
                            event.accept()
                        except Exception:
                            pass
                        return True  # не пускаем дальше в CanvasView
                except Exception:
                    # не мешаем CanvasView работать
                    pass

            # инструмент не обработал — пусть CanvasView делает своё (зум и т.п.)
            return False

        # --- МЫШЬ: отдаём активному инструменту в сцен-координатах
        if obj is self.view.viewport() and et in (
            QEvent.Type.MouseButtonPress,
            QEvent.Type.MouseButtonRelease,
            QEvent.Type.MouseMove,
        ):
            tool = getattr(self, "_active_tool", None)
            if tool and hasattr(tool, "on_mouse_event"):
                try:
                    # pos -> scene
                    pos = getattr(event, "pos", None)
                    if callable(pos):
                        scene_pos = self.view.mapToScene(event.pos())
                    else:
                        # fallback
                        from PyQt6.QtCore import QPoint

                        scene_pos = self.view.mapToScene(QPoint(0, 0))
                    # тип
                    if et == QEvent.Type.MouseButtonPress:
                        etype = "press"
                    elif et == QEvent.Type.MouseButtonRelease:
                        etype = "release"
                    else:
                        etype = "move"
                    # кнопки/модификаторы
                    button = getattr(event, "button", lambda: Qt.MouseButton.NoButton)()
                    buttons = getattr(event, "buttons", lambda: Qt.MouseButton.NoButton)()
                    modifiers = getattr(event, "modifiers", lambda: Qt.KeyboardModifier.NoModifier)()

                    from ui_new.tools.base import MouseEventCtx  # импорт локально, чтобы избежать циклов

                    ctx = MouseEventCtx(etype, button, buttons, modifiers, scene_pos)
                    handled = bool(tool.on_mouse_event(ctx))
                    if handled:
                        return True  # НЕ пускаем дальше в CanvasView
                except Exception:
                    # не мешаем CanvasView работать
                    pass
            # не обработано инструментом — оставить CanvasView по-умолчанию
            return False

        # --- КЛАВИАТУРА/шорткаты ---
        if et in (QEvent.Type.ShortcutOverride, QEvent.Type.KeyPress):
            # 1) Зум — дергаем view напрямую и ЗАВЕРШАЕМ событие
            try:
                mods = event.modifiers()
                key = event.key()
            except Exception:
                mods = Qt.KeyboardModifier.NoModifier
                key = None

            if mods & Qt.KeyboardModifier.ControlModifier:
                if key in (Qt.Key.Key_Plus, Qt.Key.Key_Equal):
                    try:
                        self.view._zoom_canvas(1.1)
                        event.accept()  # блокируем дальнейшее распространение
                    except Exception:
                        pass
                    return True
                if key == Qt.Key.Key_Minus:
                    try:
                        self.view._zoom_canvas(1 / 1.1)
                        event.accept()  # блокируем дальнейшее распространение
                    except Exception:
                        pass
                    return True
                if key == Qt.Key.Key_0:
                    try:
                        self.view._set_canvas_scale(1.0)
                        event.accept()  # блокируем дальнейшее распространение
                    except Exception:
                        pass
                    return True

            # 2) Если это один из шорткатов инструмента — пусть QShortcut обработает
            if self._is_tool_shortcut_event(event):
                # ShortcutOverride можно принять, чтобы конкуренты не съели
                if et == QEvent.Type.ShortcutOverride:
                    try:
                        event.accept()
                    except Exception:
                        pass
                    return True
                return False  # KeyPress — не мешаем, QShortcut вызовет cb

            return False

        return super().eventFilter(obj, event)
