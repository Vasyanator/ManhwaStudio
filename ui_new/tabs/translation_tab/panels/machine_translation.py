from __future__ import annotations
import inspect
import traceback
from functools import partial
from typing import Dict, Any, List

from PyQt6.QtCore import QSignalBlocker, QThread, QObject, pyqtSignal
from PyQt6.QtGui import QTextCursor
from PyQt6.QtWidgets import (
    QWidget, QVBoxLayout, QHBoxLayout, QLabel, QPushButton, QFrame,
    QLineEdit, QComboBox, QTextEdit, QCheckBox, QSpinBox
)

from ..utils import _is_deleted
from config import UserConfig

class _TranslationWorker(QObject):
    finished = pyqtSignal(int, int)  # translated, errors
    log = pyqtSignal(str)
    translated = pyqtSignal(dict, str)  # bubble_rec, translation

    def __init__(self, bubbles: List[dict], service: str, kwargs: Dict[str, Any]):
        super().__init__()
        self.bubbles = bubbles
        self.service = service
        self.kwargs = kwargs

    def run(self):
        try:
            from deep_translator import (
                GoogleTranslator, ChatGptTranslator, MicrosoftTranslator, YandexTranslator, DeeplTranslator
            )
        except Exception as e:
            traceback.print_exc()
            self.log.emit(f"deep_translator не установлен или не работает: {e}")
            self.finished.emit(0, len(self.bubbles))
            return

        classes = {
            "google": GoogleTranslator,
            "chatgpt": ChatGptTranslator,
            "microsoft": MicrosoftTranslator,
            "yandex": YandexTranslator,
            "deepl": DeeplTranslator,
        }
        cls = classes.get(self.service)
        if cls is None:
            self.log.emit("Не выбран сервис перевода.")
            self.finished.emit(0, len(self.bubbles))
            return

        try:
            translator = cls(**self.kwargs)
        except Exception as e:
            traceback.print_exc()
            self.log.emit(f"Не удалось создать переводчик: {e}")
            self.finished.emit(0, len(self.bubbles))
            return

        translated = 0
        errors = 0
        for rec in self.bubbles:
            text = str(rec.get("original_text", "") or "").strip()
            if not text:
                continue
            try:
                result = translator.translate(text)
            except Exception as e:
                errors += 1
                traceback.print_exc()
                self.log.emit(f"Ошибка для пузыря #{rec.get('id')}: {e}")
                continue
            if result is None:
                continue
            self.translated.emit(rec, str(result))
            translated += 1

        self.finished.emit(translated, errors)


class MachineTranslationPanel(QFrame):
    SERVICE_SCHEMAS: Dict[str, Dict[str, Any]] = {
        "google": {
            "title": "Google",
            "params": [],
            "required": []
        },
        "chatgpt": {
            "title": "ChatGPT",
            "params": [
                {"name": "api_key", "label": "API Key", "type": "secret", "placeholder": "sk-..."},
                {"name": "model", "label": "Модель", "type": "text", "default": "gpt-3.5-turbo"},
                {"name": "api_base", "label": "API Base URL", "type": "text", "default": ""}
            ],
            "required": ["api_key"]
        },
        "microsoft": {
            "title": "Microsoft",
            "params": [
                {"name": "api_key", "label": "API Key", "type": "secret", "placeholder": "Azure Translator key"},
                {"name": "region", "label": "Регион", "type": "text", "placeholder": "westeurope"}
            ],
            "required": ["api_key", "region"]
        },
        "yandex": {
            "title": "Yandex",
            "params": [
                {"name": "api_key", "label": "API Key", "type": "secret"},
                {"name": "format_", "label": "Формат", "type": "text", "default": "plain"}
            ],
            "required": ["api_key"]
        },
        "deepl": {
            "title": "DeepL",
            "params": [
                {"name": "api_key", "label": "API Key", "type": "secret"},
                {"name": "use_free_api", "label": "Бесплатный API", "type": "checkbox", "default": True}
            ],
            "required": ["api_key"]
        }
    }

    def __init__(self, parent: QWidget, project, canvas, model=None):
        super().__init__(parent)
        self.setFrameShape(QFrame.Shape.StyledPanel)
        self.setStyleSheet("""
            QFrame { background: #1b1b1b; border: 1px solid #444; color: #ddd; }
            QLabel { color: #ddd; }
            QLineEdit, QComboBox, QTextEdit { color: #ddd; background: #2b2b2b; border: 1px solid #555; }
            QCheckBox { color: #ddd; }
            QPushButton { background: #2b2b2b; color: #eee; border: 1px solid #555; padding: 4px 8px; }
            QPushButton:hover { background: #333; }
        """)

        self.project = project
        self.canvas = canvas
        self.model = model
        self._loading_settings = False
        self._param_widgets: Dict[str, Dict[str, QWidget]] = {}
        self._current_service: str = "google"
        self._translation_threads: List[QThread] = []
        self._translation_workers: List[_TranslationWorker] = []
        self._pending_workers = 0
        self._translated_total = 0
        self._errors_total = 0

        root = QVBoxLayout(self)

        hdr = QHBoxLayout()
        lbl = QLabel("Машинный перевод")
        lbl.setStyleSheet("font-weight:700; color:#fff;")
        btn_close = QPushButton("✕")
        btn_close.setFixedWidth(28)
        btn_close.clicked.connect(self.hide)
        hdr.addWidget(lbl)
        hdr.addStretch(1)
        hdr.addWidget(btn_close)
        root.addLayout(hdr)

        selector_row = QHBoxLayout()
        selector_row.addWidget(QLabel("Сервис:"))
        self.service_combo = QComboBox()
        for key, meta in self.SERVICE_SCHEMAS.items():
            self.service_combo.addItem(meta["title"], key)
        self.service_combo.currentIndexChanged.connect(self._on_service_changed)
        self.service_combo.setMinimumWidth(100)
        selector_row.addWidget(self.service_combo)
        selector_row.addStretch(1)
        root.addLayout(selector_row)

        langs_row = QHBoxLayout()
        langs_row.addWidget(QLabel("Исходный:"))
        self.source_edit = QLineEdit()
        self.source_edit.setPlaceholderText("auto")
        self.source_edit.editingFinished.connect(self._save_settings)
        langs_row.addWidget(self.source_edit)
        langs_row.addWidget(QLabel("Целевой:"))
        self.target_edit = QLineEdit()
        self.target_edit.setPlaceholderText("ru")
        self.target_edit.editingFinished.connect(self._save_settings)
        langs_row.addWidget(self.target_edit)
        langs_row.addStretch(1)
        root.addLayout(langs_row)

        threads_row = QHBoxLayout()
        threads_row.addWidget(QLabel("Потоки:"))
        self.threads_spin = QSpinBox()
        self.threads_spin.setRange(1, 32)
        self.threads_spin.setValue(1)
        self.threads_spin.valueChanged.connect(self._save_settings)
        threads_row.addWidget(self.threads_spin)
        threads_row.addStretch(1)
        root.addLayout(threads_row)

        self.params_box = QFrame()
        self.params_box.setFrameShape(QFrame.Shape.StyledPanel)
        self.params_box.setStyleSheet("QFrame { border: 1px solid #333; }")
        self.params_layout = QVBoxLayout(self.params_box)
        self.params_layout.setContentsMargins(8, 6, 8, 6)
        self.params_layout.setSpacing(6)
        root.addWidget(self.params_box)

        actions = QHBoxLayout()
        self.btn_translate_all = QPushButton("Перевести всё")
        self.btn_translate_page = QPushButton("Перевести на текущей странице")
        self.btn_translate_all.clicked.connect(lambda: self._start_translation(scope="all"))
        self.btn_translate_page.clicked.connect(lambda: self._start_translation(scope="page"))
        actions.addWidget(self.btn_translate_all)
        actions.addWidget(self.btn_translate_page)
        actions.addStretch(1)
        root.addLayout(actions)

        self.log = QTextEdit()
        self.log.setReadOnly(True)
        self.log.setPlaceholderText("Логи перевода будут показаны здесь...")
        root.addWidget(self.log)

        self._load_from_settings()
        self._render_params_ui(self._current_service)
        self.hide()

    def _cv(self):
        cv = getattr(self, "canvas", None)
        if cv is None or _is_deleted(cv):
            return None
        return cv

    def _append_log(self, text: str):
        existing = self.log.toPlainText()
        prefix = "" if not existing else "\n"
        self.log.setPlainText(existing + prefix + text)
        self.log.moveCursor(QTextCursor.MoveOperation.End)

    def _on_service_changed(self, idx: int):
        if self._loading_settings:
            return
        key = self.service_combo.currentData()
        if not key:
            return
        self._current_service = str(key)
        self._render_params_ui(self._current_service)
        self._save_settings()

    def _render_params_ui(self, service_key: str):
        while self.params_layout.count():
            item = self.params_layout.takeAt(0)
            if item.widget():
                item.widget().deleteLater()
        widgets: Dict[str, QWidget] = {}
        cfg_values = self._service_params_from_settings(service_key)
        for field in self.SERVICE_SCHEMAS.get(service_key, {}).get("params", []):
            name = field["name"]
            row = QHBoxLayout()
            row.addWidget(QLabel(field.get("label", name)))
            w_type = field.get("type", "text")
            if w_type == "checkbox":
                w = QCheckBox()
                default = cfg_values.get(name, field.get("default", False))
                w.setChecked(bool(default))
                w.stateChanged.connect(self._save_settings)
            else:
                w = QLineEdit()
                w.setEchoMode(QLineEdit.EchoMode.Password if field.get("type") == "secret" else QLineEdit.EchoMode.Normal)
                if "placeholder" in field:
                    w.setPlaceholderText(str(field["placeholder"]))
                val = cfg_values.get(name, field.get("default", ""))
                w.setText(str(val) if val is not None else "")
                w.editingFinished.connect(self._save_settings)
            widgets[name] = w
            if isinstance(w, QCheckBox):
                row.addWidget(w)
            else:
                row.addWidget(w)
            row.addStretch(1)
            wrapper = QFrame()
            wrapper.setFrameShape(QFrame.Shape.NoFrame)
            lay = QHBoxLayout(wrapper)
            lay.setContentsMargins(0, 0, 0, 0)
            lay.addLayout(row)
            self.params_layout.addWidget(wrapper)
        self.params_layout.addStretch(1)
        self._param_widgets[service_key] = widgets

    def _service_params_from_settings(self, service_key: str) -> Dict[str, Any]:
        settings = getattr(self.project, "settings", None)
        mt = getattr(settings, "machine_translation", None) if settings else None
        params_sources = []
        if mt:
            params_sources.append(getattr(mt, "params", None))
        user_cfg = self._user_cfg()
        if user_cfg:
            params_sources.append(getattr(user_cfg, "params", None))
        for params in params_sources:
            if hasattr(params, "_data"):
                params = getattr(params, "_data", params)
            if isinstance(params, dict):
                val = params.get(service_key, {}) or {}
                if isinstance(val, dict):
                    return dict(val)
        return {}

    def _load_from_settings(self):
        settings = getattr(self.project, "settings", None)
        mt = getattr(settings, "machine_translation", None) if settings else None
        user_cfg = self._user_cfg()
        self._loading_settings = True
        try:
            def pick(attr: str, default):
                for src in (mt, user_cfg):
                    if not src:
                        continue
                    val = getattr(src, attr, None)
                    if val not in (None, ""):
                        return val
                return default

            threads = pick("threads", 1)
            try:
                threads = max(1, int(threads))
            except Exception:
                threads = 1
            self.threads_spin.setValue(threads)
            with QSignalBlocker(self.source_edit):
                self.source_edit.setText(str(pick("source_lang", "auto")) or "auto")
            with QSignalBlocker(self.target_edit):
                self.target_edit.setText(str(pick("target_lang", "ru")) or "ru")
            service = pick("service", "google") or "google"
            self._current_service = service
            idx = self.service_combo.findData(service)
            if idx >= 0:
                with QSignalBlocker(self.service_combo):
                    self.service_combo.setCurrentIndex(idx)
        finally:
            self._loading_settings = False

    def _user_cfg(self):
        try:
            return UserConfig.TranslarionTab.MachineTranslation
        except Exception:
            return None

    def _extract_params_dict(self, params_obj) -> Dict[str, Any]:
        if isinstance(params_obj, dict):
            return dict(params_obj)
        if hasattr(params_obj, "_data"):
            return dict(getattr(params_obj, "_data", {}) or {})
        return {}

    def _save_settings(self):
        if self._loading_settings:
            return
        settings = getattr(self.project, "settings", None)
        mt = getattr(settings, "machine_translation", None) if settings else None
        service = self._current_service
        source_lang = self.source_edit.text() or "auto"
        target_lang = self.target_edit.text() or "ru"
        threads = max(1, int(self.threads_spin.value()))
        widget_map = self._param_widgets.get(service, {})

        def update_params(container):
            if not container:
                return
            params_dict = self._extract_params_dict(getattr(container, "params", None))
            current_params = params_dict.get(service, {}) if isinstance(params_dict, dict) else {}
            if not isinstance(current_params, dict):
                current_params = {}
            for name, widget in widget_map.items():
                if isinstance(widget, QCheckBox):
                    current_params[name] = bool(widget.isChecked())
                elif isinstance(widget, QLineEdit):
                    current_params[name] = widget.text()
            params_dict[service] = current_params
            container.params = params_dict

        if mt:
            mt.service = service
            mt.source_lang = source_lang
            mt.target_lang = target_lang
            mt.threads = threads
            update_params(mt)

        user_cfg = self._user_cfg()
        if user_cfg:
            user_cfg.service = service
            user_cfg.source_lang = source_lang
            user_cfg.target_lang = target_lang
            user_cfg.threads = threads
            update_params(user_cfg)
            try:
                UserConfig.save()
            except Exception:
                traceback.print_exc()

    def _set_buttons_enabled(self, enabled: bool):
        self.btn_translate_all.setEnabled(enabled)
        self.btn_translate_page.setEnabled(enabled)
        self.threads_spin.setEnabled(enabled)

    def _start_translation(self, scope: str):
        if self._pending_workers:
            self._append_log("Перевод уже выполняется.")
            return

        bubbles = self._collect_untranslated(scope)
        if not bubbles:
            self._append_log("Нет пузырей без перевода.")
            return
        self._start_translation_for_records(bubbles)

    def _start_translation_for_records(self, bubbles: List[dict]):
        if self._pending_workers:
            self._append_log("Перевод уже выполняется.")
            return
        if not bubbles:
            self._append_log("Нет пузырей для перевода.")
            return
        translator_cfg = self._build_translator_config()
        if translator_cfg is None:
            return
        _, kwargs = translator_cfg
        threads_requested = max(1, int(self.threads_spin.value()))
        chunks = self._split_bubbles(bubbles, threads_requested)
        self._translated_total = 0
        self._errors_total = 0
        self._pending_workers = 0
        self._translation_threads = []
        self._translation_workers = []
        for chunk in chunks:
            if not chunk:
                continue
            worker = _TranslationWorker(chunk, self._current_service, kwargs)
            thread = QThread(self)
            worker.moveToThread(thread)
            thread.started.connect(worker.run)
            worker.translated.connect(self._apply_translation)
            worker.log.connect(self._append_log)
            worker.finished.connect(thread.quit)
            worker.finished.connect(worker.deleteLater)
            worker.finished.connect(partial(self._on_worker_finished, worker))
            thread.finished.connect(thread.deleteLater)
            thread.finished.connect(partial(self._on_thread_finished, thread))
            self._translation_threads.append(thread)
            self._translation_workers.append(worker)
            self._pending_workers += 1
            thread.start()

        if not self._pending_workers:
            self._append_log("Нет пузырей для перевода в выбранном диапазоне.")
            return

        self._set_buttons_enabled(False)
        self._append_log(f"Старт перевода ({len(bubbles)} пузырей, потоков: {self._pending_workers})...")

    def _split_bubbles(self, bubbles: List[dict], chunks: int) -> List[List[dict]]:
        chunks = max(1, min(chunks, len(bubbles)))
        return [bubbles[i::chunks] for i in range(chunks)]

    def _on_thread_finished(self, thread: QThread):
        try:
            self._translation_threads.remove(thread)
        except ValueError:
            pass

    def _on_worker_finished(self, worker: _TranslationWorker, translated: int, errors: int):
        try:
            self._translation_workers.remove(worker)
        except ValueError:
            pass
        self._translated_total += translated
        self._errors_total += errors
        self._pending_workers = max(0, self._pending_workers - 1)
        if self._pending_workers == 0:
            self._on_translation_finished(self._translated_total, self._errors_total)
            self._reset_translation_state()

    def _on_translation_finished(self, translated: int, errors: int):
        self._append_log(f"Готово: переведено {translated} (ошибок: {errors}).")

    def _reset_translation_state(self):
        self._pending_workers = 0
        self._translation_threads = []
        self._translation_workers = []
        self._translated_total = 0
        self._errors_total = 0
        self._set_buttons_enabled(True)

    def _collect_untranslated(self, scope: str) -> List[dict]:
        current_page = None
        cv = self._cv()
        if scope == "page" and cv:
            try:
                current_page = cv.current_page_index()
            except Exception:
                try:
                    current_page = cv._current_page_idx()
                except Exception:
                    current_page = None

        items = []
        for b in list(getattr(self.project, "bubbles", []) or []):
            translation = str(b.get("text", "") or "").strip()
            original = str(b.get("original_text", "") or "").strip()
            if translation or not original:
                continue
            if b.get("img_idx") is None or b.get("img_v") is None:
                continue
            if scope == "page" and current_page is not None and int(b.get("img_idx", -1)) != int(current_page):
                continue
            items.append(b)
        return items

    def _build_translator_config(self):
        try:
            from deep_translator import (
                GoogleTranslator, ChatGptTranslator, MicrosoftTranslator, YandexTranslator, DeeplTranslator
            )
        except Exception as e:
            traceback.print_exc()
            self._append_log(f"deep_translator не установлен или не работает: {e}")
            return None

        classes = {
            "google": GoogleTranslator,
            "chatgpt": ChatGptTranslator,
            "microsoft": MicrosoftTranslator,
            "yandex": YandexTranslator,
            "deepl": DeeplTranslator,
        }
        cls = classes.get(self._current_service)
        if cls is None:
            self._append_log("Не выбран сервис перевода.")
            return None

        kwargs: Dict[str, Any] = {
            "source": self.source_edit.text() or "auto",
            "target": self.target_edit.text() or "ru",
        }
        for name, widget in self._param_widgets.get(self._current_service, {}).items():
            if isinstance(widget, QCheckBox):
                kwargs[name] = bool(widget.isChecked())
            elif isinstance(widget, QLineEdit):
                kwargs[name] = widget.text()
        required = self.SERVICE_SCHEMAS.get(self._current_service, {}).get("required", [])
        missing_filled = [field for field in required if not str(kwargs.get(field, "")).strip()]
        if missing_filled:
            self._append_log(f"Заполните параметры: {', '.join(missing_filled)}")
            return None
        try:
            sig = inspect.signature(cls.__init__)
            allowed = {p for p in sig.parameters if p != "self"}
            filtered = {k: v for k, v in kwargs.items() if k in allowed}
            missing = [
                p for p, meta in sig.parameters.items()
                if (
                    p != "self"
                    and meta.default is inspect._empty
                    and meta.kind not in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD)
                    and p not in filtered
                )
            ]
            if missing:
                self._append_log(f"Заполните параметры: {', '.join(missing)}")
                return None
            return cls, filtered
        except Exception as e:
            traceback.print_exc()
            self._append_log(f"Не удалось создать переводчик: {e}")
            return None

    def _apply_translation(self, rec: dict, translation: str):
        bid = rec.get("id")
        try:
            bid = int(bid)
        except Exception:
            pass

        # обновляем проектную запись
        target_rec = rec
        for e in getattr(self.project, "bubbles", []):
            try:
                if int(e.get("id")) == int(bid):
                    target_rec = e
                    break
            except Exception:
                continue
        if isinstance(target_rec, dict):
            target_rec["text"] = translation
            target_rec["translation_status"] = "translated"

        cv = self._cv()
        b = cv.bubbles.get(bid) if cv else None
        if b and b.text_widget:
            b.text_widget.blockSignals(True)
            b.text_widget.setPlainText(translation)
            b.text_widget.blockSignals(False)
            try:
                cv._adjust_box(bid, update_model=False)
            except Exception:
                traceback.print_exc()

        payload = {"id": bid, "text": translation, "translation_status": "translated"}
        for key in ("img_idx", "img_u", "img_v", "side"):
            if b and hasattr(b, key):
                payload[key] = getattr(b, key)
            elif isinstance(target_rec, dict) and key in target_rec:
                payload[key] = target_rec.get(key)
        if self.model:
            origin = cv.uid if cv else "machine_translation"
            self.model.update(payload, origin)
        elif hasattr(self.project, "autosave"):
            try:
                self.project.autosave()
            except Exception:
                traceback.print_exc()

        if cv:
            cv.bubblesChanged.emit("text", bid)
