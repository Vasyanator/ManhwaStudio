from __future__ import annotations
from typing import Dict
import traceback

from PyQt6.QtCore import QSignalBlocker, QThread, QObject, pyqtSignal
from PyQt6.QtWidgets import QWidget, QVBoxLayout, QHBoxLayout, QLabel, QPushButton, QCheckBox, QFrame

from ..ocr.base import OcrEngineBase

STATUS_HINTS = {
    "idle": "Выберите OCR, его настройки, и нажмите Загрузить.",
    "loading": "OCR может скачивать модели, это займет некоторое время. Прогресс смотрите в консоли.",
    "ok": "OCR загружен и готов к работе. Используйте Shift+ЛКМ для выделения области",
    "error": "Ошибка при загрузке OCR. Проверьте консоль. Если непонятно, попросите ИИ объяснить ошибку.",
    "default": "OCR может скачивать модели, это займет некоторое время. Прогресс смотрите в консоли."
}

class _OcrLoader(QObject):
    """Фоновая загрузка OCR, чтобы не блокировать UI."""
    finished = pyqtSignal(bool)

    def __init__(self, engine: OcrEngineBase | None):
        super().__init__()
        self.engine = engine

    def run(self):
        ok = True
        try:
            if self.engine:
                ok = self.engine.ensure_loaded()
                if ok:
                    try:
                        self.engine.warmup()
                    except Exception:
                        traceback.print_exc()
        except Exception:
            traceback.print_exc()
            ok = False
        self.finished.emit(ok)

class SettingsPanel(QFrame):
    """
    Панель настроек OCR для вкладки Translation.

    Предоставляет интерфейс для:
    - Выбора OCR движка (EasyOCR/PaddleOCR/MangaOCR)
    - Конфигурации языков распознавания
    - Настройки параметров GPU/CPU
    - Опций постобработки (копирование, создание пузырей)
    """

    ocrStateChanged = pyqtSignal(str)

    # Инициализация панели и построение UI
    def __init__(self, parent: QWidget, canvas, project_settings = None):
        super().__init__(parent)
        self.setFrameShape(QFrame.Shape.StyledPanel)
        self.setStyleSheet("""
            QFrame { background: #202020; border: 1px solid #444; color: #ddd; }
            QLabel { color: #ddd; }
            QCheckBox, QLineEdit, QComboBox, QSpinBox { color: #ddd; background: #2b2b2b; border: 1px solid #555; }
            QPushButton { background: #2b2b2b; color: #eee; border: 1px solid #555; padding: 4px 8px; }
            QPushButton:hover { background: #333; }
        """)
        self.canvas = canvas
        self.project_settings = project_settings
        self.engines: Dict[str, OcrEngineBase] = getattr(self.canvas, "ocr_engines", {})
        self._loading_settings = False
        self._selected_engine: str = "easyocr" if "easyocr" in self.engines else (next(iter(self.engines.keys())) if self.engines else "none")
        self._loader_thread: QThread | None = None
        self._loader_worker: _OcrLoader | None = None
        self._load_task_id = 0
        self._engine_state: str = "idle"
        lay = QVBoxLayout(self)
        hdr = QHBoxLayout()
        title = QLabel("Настройки OCR")
        title.setStyleSheet("font-weight:700;color:#fff;")
        btn_close = QPushButton("✕")
        btn_close.setFixedWidth(28)
        btn_close.clicked.connect(self.hide)
        hdr.addWidget(title)
        hdr.addStretch(1)
        hdr.addWidget(btn_close)
        lay.addLayout(hdr)

        st_lay = QHBoxLayout()
        st_lbl = QLabel("Статус:")
        self.lbl_status = QLabel("не загружен")
        self.lbl_status.setStyleSheet("color: gray; font-weight: 600;")
        st_lay.addWidget(st_lbl)
        st_lay.addWidget(self.lbl_status)
        st_lay.addStretch(1)
        lay.addLayout(st_lay)
        self.lbl_status_hint = QLabel(STATUS_HINTS.get("idle", ""))
        self.lbl_status_hint.setStyleSheet("color: #888; font-size: 11px;")
        lay.addWidget(self.lbl_status_hint)

        self.chk_none = QCheckBox("Нет")
        self.chk_easy = QCheckBox("EasyOCR")
        self.chk_easy.setToolTip("Более простой и универсальный OCR, поддерживает много языков")
        self.chk_paddle = QCheckBox("PaddleOCR")
        self.chk_paddle.setToolTip("Продвинутый китайский OCR движок. Хорош для Китайского и Корейского, и частично Японского и Английского")
        self.chk_manga = QCheckBox("MangaOCR")
        self.chk_manga.setToolTip("Японский OCR для манги, без настроек")
        self._engine_checkboxes = {
            "none": self.chk_none,
            "easyocr": self.chk_easy,
            "paddle": self.chk_paddle,
            "mangaocr": self.chk_manga,
        }
        eng_lay = QHBoxLayout()
        eng_lay.addWidget(self.chk_none)
        eng_lay.addWidget(self.chk_easy)
        eng_lay.addWidget(self.chk_paddle)
        eng_lay.addWidget(self.chk_manga)
        eng_lay.addStretch(1)
        lay.addLayout(eng_lay)

        for key, box in self._engine_checkboxes.items():
            box.stateChanged.connect(lambda _, k=key: self._on_engine_changed(k))

        self.engine_ui_container = QFrame()
        self.engine_ui_container.setFrameShape(QFrame.Shape.NoFrame)
        self.engine_ui_layout = QVBoxLayout(self.engine_ui_container)
        self.engine_ui_layout.setContentsMargins(0, 0, 0, 0)
        self.engine_ui_layout.setSpacing(6)
        lay.addWidget(self.engine_ui_container)

        btn_reload = QPushButton("🔄  Загрузить")
        btn_reload.clicked.connect(self._apply_settings)
        lay.addWidget(btn_reload)

        general_box = QFrame()
        general_box.setFrameShape(QFrame.Shape.StyledPanel)
        general_box.setStyleSheet("QFrame { border: 1px solid #333; }")
        general_lay = QVBoxLayout(general_box)
        general_lay.setContentsMargins(8, 6, 8, 6)
        general_lay.setSpacing(4)
        gen_title = QLabel("Общие настройки (применяются сразу)")
        gen_title.setStyleSheet("font-weight:600;")
        self.chk_join = QCheckBox("Сохранять переносы строк")
        self.chk_reflect = QCheckBox("Столбцы справа налево (манга)")
        self.chk_reflect.setToolTip("Отразить порядок OCR-строк (последняя станет первой)")
        self.chk_copy = QCheckBox("Копировать в буфер")
        self.chk_bubble = QCheckBox("Создавать пузырь")
        general_lay.addWidget(gen_title)
        general_lay.addWidget(self.chk_join)
        general_lay.addWidget(self.chk_reflect)
        general_lay.addWidget(self.chk_copy)
        general_lay.addWidget(self.chk_bubble)
        general_box.setContentsMargins(0, 0, 0, 0)
        lay.addWidget(general_box)

        tip = QLabel("Shift+ЛКМ — выделить область для OCR.")
        tip.setStyleSheet("color:#888;")
        lay.addWidget(tip)
        lay.addStretch(1)

        self._connect_settings_signals()
        self._load_from_settings()
        self._render_engine_ui(self._selected_engine)
        self._update_engine_state_label("не загружен", "gray", state="idle")
        self.hide()

    # Управление состоянием и применение настроек
    def _update_engine_state_label(self, text, color, state: str | None = None):
        if state is not None:
            self._engine_state = state
            try:
                self.ocrStateChanged.emit(state)
            except Exception:
                traceback.print_exc()
        self.lbl_status.setText(text)
        self.lbl_status.setStyleSheet(f"color: {color}; font-weight:600;")
        hint = STATUS_HINTS.get(state or "default", STATUS_HINTS.get("default", ""))
        if hint:
            self.lbl_status_hint.setText(hint)

    def ocr_state(self) -> str:
        return self._engine_state

    def _set_engine_checkboxes(self, engine: str):
        for key, box in self._engine_checkboxes.items():
            with QSignalBlocker(box):
                box.setChecked(key == engine)

    def _on_engine_changed(self, engine: str):
        if self._loading_settings:
            return
        box = self._engine_checkboxes.get(engine)
        if box is not None and not box.isChecked():
            # Не даём оставить движок без выбора кликом по активному чекбоксу
            with QSignalBlocker(box):
                box.setChecked(True)
            return

        # Эксклюзивный выбор
        self._set_engine_checkboxes(engine)
        self._selected_engine = engine
        if engine == "none":
            self._update_engine_state_label("не загружен", "gray", state="idle")
        else:
            self._update_engine_state_label("не загружен", "gray", state="idle")

        self._render_engine_ui(engine)
        self._save_settings()

    def _start_engine_load(self, engine_key: str):
        """Запускает фоновой поток загрузки выбранного движка."""
        self._load_task_id += 1
        task_id = self._load_task_id

        if self._loader_thread:
            try:
                self._loader_thread.quit()
                self._loader_thread.wait()
            except Exception:
                traceback.print_exc()
            self._loader_thread = None
            self._loader_worker = None

        engine_obj = self._get_engine(engine_key)
        thread = QThread(self)
        worker = _OcrLoader(engine_obj)
        worker.moveToThread(thread)

        thread.started.connect(worker.run)
        worker.finished.connect(lambda ok, tid=task_id, eng=engine_key: self._on_engine_loaded(tid, eng, ok))
        worker.finished.connect(thread.quit)
        worker.finished.connect(worker.deleteLater)
        thread.finished.connect(thread.deleteLater)
        thread.finished.connect(lambda t=thread: self._set_loader_thread(t, None, None))

        self._loader_thread = thread
        self._loader_worker = worker
        thread.start()

    def _set_loader_thread(self, thread_to_clear: QThread | None, new_thread: QThread | None, new_worker: _OcrLoader | None):
        if thread_to_clear is None or self._loader_thread is thread_to_clear:
            self._loader_thread = new_thread
            self._loader_worker = new_worker

    def _on_engine_loaded(self, task_id: int, engine: str, ok: bool):
        # Если пока грузилось, пользователь переключился на другой движок — игнорируем результат.
        if task_id != self._load_task_id or engine != (self._current_engine() or ""):
            return

        # Сброс ссылки на текущего воркера (поток очистится по finished)
        self._set_loader_thread(self._loader_thread, self._loader_thread, None)

        if ok:
            self._update_engine_state_label("загружен", "lightgreen", state="ok")
        else:
            self._update_engine_state_label("ошибка", "red", state="error")

    def _render_engine_ui(self, engine: str):
        while self.engine_ui_layout.count():
            item = self.engine_ui_layout.takeAt(0)
            w = item.widget()
            if w:
                w.setParent(None)

        eng = self._get_engine(engine)
        if eng:
            try:
                widget = eng.ui(self.engine_ui_container, self._on_engine_param_changed)
                self.engine_ui_layout.addWidget(widget)
            except Exception:
                traceback.print_exc()
        else:
            placeholder = QLabel("OCR не выбран")
            placeholder.setStyleSheet("color:#888;")
            self.engine_ui_layout.addWidget(placeholder)

    def _get_engine(self, key: str) -> OcrEngineBase | None:
        return self.engines.get(key)

    def _apply_settings(self):
        engine_key = self._selected_engine or (self._current_engine() or "none")
        engine = self._get_engine(engine_key)

        if engine is None and engine_key not in ("none",):
            self.canvas.ocr_engine = "none"
            self._update_engine_state_label("ошибка", "red", state="error")
            return

        if engine:
            engine.read_ui_state()
            if not engine.validate():
                self._update_engine_state_label("ошибка", "red", state="error")
                return

        self.canvas.ocr_engine = engine_key or "none"
        self._apply_general_settings()

        if engine and self.canvas.ocr_engine != "none":
            engine.reset()
            self._update_engine_state_label("Загрузка...", "#ffae42", state="loading")
            self._start_engine_load(self.canvas.ocr_engine)
        else:
            self._update_engine_state_label("загружен", "lightgreen", state="ok")

        print(f"[OCR] {self.canvas.ocr_engine} настройки применены")
        self._save_settings()

    def _connect_settings_signals(self):
        """Подключаем автосохранение при любом изменении UI."""
        self.chk_join.stateChanged.connect(self._on_general_setting_changed)
        self.chk_reflect.stateChanged.connect(self._on_general_setting_changed)
        self.chk_copy.stateChanged.connect(self._on_general_setting_changed)
        self.chk_bubble.stateChanged.connect(self._on_general_setting_changed)

    def _current_engine(self) -> str | None:
        if self.chk_none.isChecked():
            return "none"
        if self.chk_easy.isChecked():
            return "easyocr"
        if self.chk_paddle.isChecked():
            return "paddle"
        if self.chk_manga.isChecked():
            return "mangaocr"
        return None

    def _on_engine_param_changed(self):
        if self._loading_settings:
            return
        engine = self._get_engine(self._selected_engine)
        if engine:
            engine.read_ui_state()
        self._save_settings()

    def _apply_general_settings(self):
        self.canvas.join_newlines = self.chk_join.isChecked()
        self.canvas.post_copy = self.chk_copy.isChecked()
        self.canvas.post_bubble = self.chk_bubble.isChecked()
        self.canvas.post_reflect_strings = self.chk_reflect.isChecked()

    def _on_general_setting_changed(self):
        if self._loading_settings:
            return
        self._apply_general_settings()
        self._save_settings()

    def ensure_ocr_from_config(self) -> str:
        """
        Подгружает настройки OCR из конфига и при необходимости запускает загрузку движка.
        Возвращает одно из состояний: 'ok' | 'loading' | 'none' | 'error'.
        """
        self._load_from_settings()
        self._render_engine_ui(self._selected_engine)

        engine_key = self._selected_engine or (self._current_engine() or "none")
        if engine_key == "none":
            self.canvas.ocr_engine = "none"
            self._apply_general_settings()
            self._update_engine_state_label("не загружен", "gray", state="idle")
            return "none"

        engine = self._get_engine(engine_key)
        if engine is None:
            self.canvas.ocr_engine = "none"
            self._update_engine_state_label("ошибка", "red", state="error")
            return "error"

        self.canvas.ocr_engine = engine_key
        self._apply_general_settings()

        if getattr(engine, "_loaded", False):
            self._update_engine_state_label("загружен", "lightgreen", state="ok")
            return "ok"

        if self._loader_thread is not None:
            self._update_engine_state_label("Загрузка...", "#ffae42", state="loading")
            return "loading"

        if getattr(engine, "_ui", None) is not None:
            engine.read_ui_state()
        if not engine.validate():
            self._update_engine_state_label("ошибка", "red", state="error")
            return "error"

        engine.reset()
        self._update_engine_state_label("Загрузка...", "#ffae42", state="loading")
        self._start_engine_load(self.canvas.ocr_engine)
        self._save_settings()
        return "loading"

    @staticmethod
    def _normalize_engine_key(engine_key: str) -> str:
        key = (engine_key or "").strip().lower()
        if key in ("paddleocr",):
            return "paddle"
        if key in ("easy",):
            return "easyocr"
        if key in ("mocr", "manga_ocr", "manga"):
            return "mangaocr"
        return key

    @staticmethod
    def _as_dict(value):
        if isinstance(value, dict):
            return value
        data = getattr(value, "_data", None)
        if isinstance(data, dict):
            return data
        return {}

    def _extract_legacy_value(self, raw_value, engine_key: str):
        if isinstance(raw_value, dict):
            aliases = {
                "easyocr": ("easyocr", "easy"),
                "paddle": ("paddle", "paddleocr"),
                "mangaocr": ("mangaocr", "manga_ocr", "manga", "mocr"),
                "none": ("none",),
            }
            for alias in aliases.get(engine_key, (engine_key,)):
                if alias in raw_value:
                    return raw_value.get(alias)
            return None
        return raw_value

    def _load_from_settings(self):
        """Читает project_settings и проставляет значения в UI."""
        ocr = getattr(self.project_settings, "OCR", None) if self.project_settings else None
        if not ocr:
            self._loading_settings = True
            try:
                self._set_engine_checkboxes(self._selected_engine)
                self.chk_join.setChecked(bool(getattr(self.canvas, "join_newlines", True)))
                self.chk_reflect.setChecked(bool(getattr(self.canvas, "post_reflect_strings", False)))
                self.chk_copy.setChecked(bool(getattr(self.canvas, "post_copy", False)))
                self.chk_bubble.setChecked(bool(getattr(self.canvas, "post_bubble", True)))
            finally:
                self._loading_settings = False
            self._apply_general_settings()
            return

        # Не перезаписываем конфиг во время первичного заполнения UI
        self._loading_settings = True

        # Движок
        engine = getattr(ocr, "engine", None) or self._selected_engine or "none"
        try:
            raw_params = getattr(ocr, "params", None)
            params_by_engine_raw = self._as_dict(raw_params)
            params_by_engine = {}
            for raw_key, raw_payload in params_by_engine_raw.items():
                normalized_key = self._normalize_engine_key(str(raw_key))
                if not normalized_key:
                    continue
                params_by_engine[normalized_key] = self._as_dict(raw_payload)

            legacy_langs = getattr(ocr, "langs", None)
            legacy_gpu = getattr(ocr, "gpu", None)

            for key, eng in self.engines.items():
                payload = params_by_engine.get(key, {})
                legacy_lang = self._extract_legacy_value(legacy_langs, key)
                legacy_gpu_value = self._extract_legacy_value(legacy_gpu, key)
                if not payload and (legacy_lang is not None or legacy_gpu_value is not None):
                    payload = {}
                    if legacy_lang is not None:
                        payload["langs"] = legacy_lang
                    if legacy_gpu_value is not None:
                        payload["gpu"] = legacy_gpu_value
                eng.load_settings(payload if isinstance(payload, dict) else {})

            normalized_engine = self._normalize_engine_key(str(engine))
            if normalized_engine not in self._engine_checkboxes:
                normalized_engine = "none"
            self._selected_engine = normalized_engine or "none"
            self._set_engine_checkboxes(self._selected_engine)

            self.chk_join.setChecked(bool(getattr(ocr, "join", True)))
            self.chk_reflect.setChecked(bool(getattr(ocr, "reflect", False)))
            self.chk_copy.setChecked(bool(getattr(ocr, "copy", False)))
            self.chk_bubble.setChecked(bool(getattr(ocr, "bubbles", True)))
        finally:
            self._loading_settings = False
        self._apply_general_settings()
        self._update_engine_state_label("не загружен", "gray", state="idle")

    def _save_settings(self):
        """Считывает текущее состояние UI и сохраняет в project_settings (автосейв через NestedConfig)."""
        if self._loading_settings:
            return
        ocr = getattr(self.project_settings, "OCR", None) if self.project_settings else None
        if not ocr:
            return

        engine = self._current_engine()
        selected_engine_obj = self._get_engine(self._selected_engine)
        if selected_engine_obj and getattr(selected_engine_obj, "_ui", None) is not None:
            selected_engine_obj.read_ui_state()
        if engine:
            ocr.engine = engine
        ocr.join = self.chk_join.isChecked()
        ocr.reflect = self.chk_reflect.isChecked()
        ocr.copy = self.chk_copy.isChecked()
        ocr.bubbles = self.chk_bubble.isChecked()
        params_payload = {}
        for key, eng in self.engines.items():
            params_payload[key] = eng.save_settings()
        ocr.params = params_payload
