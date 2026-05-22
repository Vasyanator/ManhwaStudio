from __future__ import annotations
from typing import Dict, Optional, List
import traceback

from PyQt6.QtCore import QTimer
from PyQt6.QtWidgets import (
    QWidget, QVBoxLayout, QHBoxLayout, QLabel, QPushButton, QScrollArea,
    QSpacerItem, QSizePolicy, QTextEdit, QFrame, QLineEdit, QCheckBox,
    QComboBox, QSpinBox
)
from PyQt6.QtGui import QGuiApplication

from modules.utils_qt import safe_disconnect
from ..utils import _is_deleted

class BubblesPanel(QFrame):
    """
    Панель управления пузырями для вкладки Translation.

    Предоставляет интерфейс для:
    - Просмотра списка всех пузырей с их статусом
    - Редактирования текста пузырей
    - Операций с пузырями (копирование, вставка, перемещение, удаление)
    - Синхронизации с canvas и моделью через debounce
    """

    def __init__(self, parent: QWidget, project, canvas, model):
        super().__init__(parent)
        self.setFrameShape(QFrame.Shape.StyledPanel)
        self.setStyleSheet("""
            QFrame { background: #202020; border: 1px solid #444; color: #ddd; }
            QLabel { color: #ddd; }
            QTextEdit { color: #ddd; background: #2b2b2b; border: 1px solid #555; font-family: monospace; }
            QLineEdit, QComboBox, QSpinBox { color: #ddd; background: #2b2b2b; border: 1px solid #555; }
            QPushButton { background: #2b2b2b; color: #eee; border: 1px solid #555; padding: 4px 8px; }
            QPushButton:hover { background: #333; }
        """)

        self.project = project
        self.canvas = canvas
        self.model = model

        root = QVBoxLayout(self)
        hdr = QHBoxLayout()
        lbl = QLabel("Пузыри")
        lbl.setStyleSheet("font-weight:700;color:#fff;")
        btn_refresh = QPushButton("Обновить")
        btn_close = QPushButton("✕"); btn_close.setFixedWidth(28)
        btn_refresh.clicked.connect(self.rebuild_now)
        btn_close.clicked.connect(self.hide)
        hdr.addWidget(lbl); hdr.addStretch(1); hdr.addWidget(btn_refresh); hdr.addWidget(btn_close)
        root.addLayout(hdr)

        search_row = QHBoxLayout()
        self._search_input = QLineEdit()
        self._search_input.setPlaceholderText("Поиск...")
        self._search_input.setMinimumWidth(180)

        self._page_filter = QComboBox()
        self._page_filter.setMinimumWidth(140)
        self._page_filter.setMaxVisibleItems(12)

        self._character_filter = QComboBox()
        self._character_filter.setMinimumWidth(170)
        self._character_filter.setMaxVisibleItems(12)
        self._character_filter.setEditable(True)
        self._character_filter.setInsertPolicy(QComboBox.InsertPolicy.NoInsert)

        self._search_scope = QComboBox()
        self._search_scope.addItem("Везде", "all")
        self._search_scope.addItem("Оригинал", "original")
        self._search_scope.addItem("Перевод", "translation")
        self._search_scope.setMinimumWidth(120)

        btn_find = QPushButton("Найти")
        btn_find.clicked.connect(self._apply_search_filters)

        search_row.addWidget(self._search_input, 1)
        search_row.addWidget(self._page_filter)
        search_row.addWidget(self._character_filter)
        search_row.addWidget(self._search_scope)
        search_row.addWidget(btn_find)
        root.addLayout(search_row)

        self.scroll = QScrollArea()
        self.scroll.setFrameShape(QFrame.Shape.NoFrame)
        self.scroll.setWidgetResizable(True)
        self._list_host = QWidget()
        from PyQt6.QtWidgets import QVBoxLayout as _VBL
        self._list_layout = _VBL(self._list_host)
        self._list_layout.setSpacing(6)
        self._list_layout.setContentsMargins(6, 6, 6, 6)
        self.scroll.setWidget(self._list_host)
        root.addWidget(self.scroll, 1)

        # debounce для перестроения
        self._rebuild_debounce = QTimer(self)
        self._rebuild_debounce.setInterval(30)
        self._rebuild_debounce.setSingleShot(True)
        self._rebuild_debounce.timeout.connect(self.rebuild_now)

        # debounce обновления модели из карточек
        self._panel_text_update_timer = QTimer(self)
        self._panel_text_update_timer.setInterval(300)
        self._panel_text_update_timer.setSingleShot(True)
        self._panel_pending_updates: Dict[int, str] = {}
        self._panel_text_update_timer.timeout.connect(self._flush_panel_text_updates)

        self._cards: Dict[int, QWidget] = {}
        self._applied_filters = {
            "query": "",
            "page": None,
            "character": None,
            "scope": "all",
        }

        self._refresh_filter_options([])
        self.hide()

    # Публичный API для запланированного обновления списка
    def schedule_rebuild(self):
        if not self._rebuild_debounce.isActive():
            self._rebuild_debounce.start()

    # Немедленное обновление списка пузырей
    def rebuild_now(self):
        self._clear_list()
        bubbles = list(getattr(self.project, "bubbles", []))
        try:
            bubbles.sort(key=lambda e: int(e.get("id")))
        except Exception:
            traceback.print_exc()

        self._refresh_filter_options(bubbles)
        visible = self._filter_bubbles(bubbles)

        for rec in visible:
            card = self._make_bubble_card(rec)
            try:
                self._cards[int(rec.get("id"))] = card
            except Exception:
                pass
            self._list_layout.addWidget(card)

        self._list_layout.addItem(QSpacerItem(0, 0, QSizePolicy.Policy.Minimum, QSizePolicy.Policy.Expanding))

    # Очистка списка пузырей перед перестроением
    def _clear_list(self):
        self._cards.clear()
        while self._list_layout.count():
            item = self._list_layout.takeAt(0)
            w = item.widget()
            if w is not None:
                w.deleteLater()

    # Создание карточки для отдельного пузыря с элементами управления
    def _make_bubble_card(self, rec: dict) -> QWidget:
        bid = int(rec.get("id"))
        placed = not (rec.get("img_idx") is None or rec.get("img_u") is None or rec.get("img_v") is None or rec.get("side") is None)
        title = f"Изображение #{int(rec.get('img_idx', 0))+1}" if placed else "Не привязан"

        card = QFrame()
        card.setFrameShape(QFrame.Shape.StyledPanel)
        card.setStyleSheet("QFrame{background:#1f1f1f;border:1px solid #333;border-radius:6px;}")

        v = QVBoxLayout(card)
        hdr = QHBoxLayout()
        lbl = QLabel(title)
        if not placed:
            lbl.setStyleSheet("color:red;")
        hdr.addWidget(lbl)
        v.addLayout(hdr)

        txt = QTextEdit()
        txt.setPlainText(str(rec.get("text", "")))
        v.addWidget(txt)

        meta_block = QWidget()
        meta_layout = QVBoxLayout(meta_block)
        meta_layout.setContentsMargins(0, 0, 0, 0)
        meta_layout.setSpacing(4)

        original_label = QLabel("Оригинал:")
        original_label.setStyleSheet("color:#aaa;")
        meta_layout.addWidget(original_label)

        original_text_edit = QTextEdit()
        original_text_edit.setPlaceholderText("Оригинальный текст...")
        original_text_edit.setFixedHeight(56)
        original_text_edit.setPlainText(str(rec.get("original_text", "") or ""))
        meta_layout.addWidget(original_text_edit)

        row_meta = QHBoxLayout()
        row_meta.setSpacing(6)
        row_meta.addWidget(QLabel("Порядок:"))
        spin_order = QSpinBox()
        spin_order.setRange(0, 100000)
        spin_order.setValue(int(rec.get("bubble_order", 0) or 0))
        spin_order.setFixedHeight(28)
        spin_order.setMinimumWidth(60)
        row_meta.addWidget(spin_order)

        chk_character = QCheckBox("И.П.")
        chk_character.setToolTip("Использовать готовые имена персонажей, или ввести своё.")
        row_meta.addWidget(chk_character)
        row_meta.addStretch(1)
        meta_layout.addLayout(row_meta)

        row_character = QHBoxLayout()
        row_character.setSpacing(6)
        combo_character = QComboBox()
        combo_character.setMaxVisibleItems(7)
        combo_character.setMinimumWidth(120)
        row_character.addWidget(combo_character)

        btn_refresh_chars = QPushButton("↻")
        btn_refresh_chars.setToolTip("Обновить список персонажей из characters.json")
        btn_refresh_chars.setFixedSize(28, 28)
        row_character.addWidget(btn_refresh_chars)

        edit_character = QLineEdit()
        edit_character.setPlaceholderText("Имя персонажа...")
        edit_character.setMinimumWidth(120)
        row_character.addWidget(edit_character)

        edit_clarification = QLineEdit()
        edit_clarification.setPlaceholderText("Уточнение...")
        edit_clarification.setMinimumWidth(100)
        row_character.addWidget(edit_clarification)
        row_character.addStretch(1)
        meta_layout.addLayout(row_character)

        v.addWidget(meta_block)

        def _save_field(field: str, value):
            cv = self._cv()
            if cv and hasattr(cv, "_save_bubble_field"):
                cv._save_bubble_field(bid, field, value)

        def _reload_character_items(current_name: str = ""):
            cv = self._cv()
            names = getattr(cv, "_character_names", []) if cv else []
            items = names if names else ["(нет персонажей)"]
            combo_character.blockSignals(True)
            combo_character.clear()
            combo_character.addItems(items)
            if current_name:
                idx = combo_character.findText(current_name)
                if idx >= 0:
                    combo_character.setCurrentIndex(idx)
            combo_character.blockSignals(False)

        is_known_character = rec.get("is_known_character", True)
        character_name = str(rec.get("character_name", "") or "")
        clarification = str(rec.get("clarification", "") or "")

        chk_character.setChecked(bool(is_known_character))
        _reload_character_items(character_name if is_known_character else "")
        if not is_known_character:
            edit_character.setText(character_name)
        if clarification:
            edit_clarification.setText(clarification)

        def _apply_character_mode(known: bool):
            combo_character.setVisible(known)
            btn_refresh_chars.setVisible(known)
            edit_clarification.setVisible(known)
            edit_character.setVisible(not known)

        _apply_character_mode(bool(is_known_character))

        def _on_known_changed(state: int):
            known = state == 2
            _apply_character_mode(known)
            _save_field("is_known_character", known)
            if known:
                name = combo_character.currentText()
                if name == "(нет персонажей)":
                    name = ""
                _save_field("character_name", name)
            else:
                _save_field("character_name", edit_character.text())

        def _on_combo_changed(text: str):
            name = text if text != "(нет персонажей)" else ""
            _save_field("character_name", name)
            edit_clarification.blockSignals(True)
            edit_clarification.clear()
            edit_clarification.blockSignals(False)
            _save_field("clarification", "")

        def _on_refresh():
            cv = self._cv()
            current = combo_character.currentText() if chk_character.isChecked() else ""
            if cv and hasattr(cv, "_on_refresh_characters"):
                cv._on_refresh_characters(bid)
            _reload_character_items(current)

        original_text_edit.textChanged.connect(lambda: _save_field("original_text", original_text_edit.toPlainText()))
        spin_order.valueChanged.connect(lambda val: _save_field("bubble_order", int(val)))
        chk_character.stateChanged.connect(_on_known_changed)
        combo_character.currentTextChanged.connect(_on_combo_changed)
        edit_character.textChanged.connect(lambda text: _save_field("character_name", text))
        edit_clarification.textChanged.connect(lambda text: _save_field("clarification", text))
        btn_refresh_chars.clicked.connect(_on_refresh)

        row = QHBoxLayout()
        btn_copy = QPushButton("Копировать")
        btn_paste = QPushButton("Заменить")
        btn_move = QPushButton("Переместить" if placed else "Разместить")
        btn_translate = QPushButton("Перевести")
        btn_del = QPushButton("Удалить")
        row.addWidget(btn_copy); row.addWidget(btn_paste); row.addWidget(btn_move); row.addWidget(btn_translate); row.addWidget(btn_del)
        row.addStretch(1)
        v.addLayout(row)

        def _sync_text_to_model():
            cv = self._cv()
            if not cv:
                card.setDisabled(True)
                return
            content = txt.toPlainText()

            b = getattr(cv, "bubbles", {}).get(bid)
            if b and b.text_widget:
                if b.text_widget.toPlainText() != content:
                    b.text_widget.blockSignals(True)
                    b.text_widget.setPlainText(content)
                    b.text_widget.blockSignals(False)
                    try:
                        cv._adjust_box(bid, update_model=False)
                    except Exception:
                        traceback.print_exc()

            for e in getattr(self.project, "bubbles", []):
                try:
                    if int(e.get("id")) == bid:
                        e["text"] = content
                        break
                except Exception:
                    traceback.print_exc()
            if hasattr(self.project, "autosave"):
                try:
                    self.project.autosave()
                except Exception:
                    traceback.print_exc()

            self._panel_pending_updates[bid] = content
            self._panel_text_update_timer.start()

        txt.textChanged.connect(_sync_text_to_model)

        btn_copy.clicked.connect(lambda: QGuiApplication.clipboard().setText(txt.toPlainText()))

        def _paste_and_mark_translated():
            # Вставляем текст из буфера
            txt.setPlainText(QGuiApplication.clipboard().text())

            # Обновляем translation_status на "translated"
            for e in getattr(self.project, "bubbles", []):
                try:
                    if int(e.get("id")) == bid:
                        e['translation_status'] = 'translated'
                        break
                except Exception:
                    traceback.print_exc()

            # Автосохранение
            if hasattr(self.project, "autosave"):
                try:
                    self.project.autosave()
                except Exception:
                    traceback.print_exc()

            # Обновляем модель
            if self.model:
                cv = self._cv()
                if cv:
                    b = cv.bubbles.get(bid)
                    if b:
                        rec = {
                            'id': bid,
                            'translation_status': 'translated',
                            'img_idx': b.img_idx,
                            'img_u': b.img_u,
                            'img_v': b.img_v,
                            'side': b.side
                        }
                        self.model.update(rec, cv.uid)

                    # Обновляем визуальное состояние кнопки статуса (если есть метод)
                    if hasattr(cv, '_update_translation_button_visual'):
                        cv._update_translation_button_visual(bid, 'translated')

        btn_paste.clicked.connect(_paste_and_mark_translated)

        def _move():
            cv = self._cv()
            if not cv:
                card.setDisabled(True)
                return
            if hasattr(cv, "toggle_move_mode"):
                cv.toggle_move_mode(bid)
            else:
                cv._move_active_bid = bid
        btn_move.clicked.connect(_move)

        def _delete():
            cv = self._cv()
            if cv:
                try:
                    cv.delete_bubble_by_id(bid)
                except Exception:
                    traceback.print_exc()
            self.rebuild_now()
        btn_del.clicked.connect(_delete)

        def _translate():
            cv = self._cv()
            if not cv:
                card.setDisabled(True)
                return
            if hasattr(cv, "_on_translate_bubble"):
                try:
                    cv._on_translate_bubble(bid)
                except Exception:
                    traceback.print_exc()
        btn_translate.clicked.connect(_translate)

        def _exists():
            return any(str(e.get("id")) == str(bid) for e in getattr(self.project, "bubbles", []))

        def _on_bubbles_changed(*_):
            if _is_deleted(card):
                safe_disconnect(self.canvas, "bubblesChanged", _on_bubbles_changed)
                return
            if not _exists():
                card.setDisabled(True)

        self.canvas.bubblesChanged.connect(_on_bubbles_changed)
        card.destroyed.connect(lambda *_: safe_disconnect(self.canvas, "bubblesChanged", _on_bubbles_changed))

        return card

    def _get_bubble_record(self, bid: int) -> Optional[dict]:
        for e in getattr(self.project, "bubbles", []):
            try:
                if int(e.get("id")) == int(bid):
                    return e
            except Exception:
                continue
        return None

    def rebuild_card(self, bid: int) -> None:
        if not self._list_layout:
            return
        rec = self._get_bubble_record(bid)
        card = self._cards.get(int(bid))
        is_visible = rec is not None and self._matches_applied_filters(rec)
        if card is None:
            if rec is None or not is_visible:
                return
            if self._list_layout.count() == 0:
                return self.rebuild_now()
            new_card = self._make_bubble_card(rec)
            self._cards[int(bid)] = new_card
            self._list_layout.insertWidget(self._list_layout.count() - 1, new_card)
            return

        idx = self._list_layout.indexOf(card)
        if idx < 0:
            return self.rebuild_now()

        if rec is None or not is_visible:
            item = self._list_layout.takeAt(idx)
            w = item.widget()
            if w is not None:
                w.deleteLater()
            self._cards.pop(int(bid), None)
            return

        new_card = self._make_bubble_card(rec)
        self._cards[int(bid)] = new_card
        item = self._list_layout.takeAt(idx)
        w = item.widget()
        if w is not None:
            w.deleteLater()
        self._list_layout.insertWidget(idx, new_card)

    def _apply_search_filters(self):
        self._applied_filters["query"] = self._search_input.text().strip()
        self._applied_filters["page"] = self._page_filter.currentData()
        character = self._character_filter.currentData()
        if character is None:
            typed = self._character_filter.currentText().strip()
            if typed and typed != "Все персонажи":
                character = typed
        self._applied_filters["character"] = character
        self._applied_filters["scope"] = self._search_scope.currentData() or "all"
        self.rebuild_now()

    def _refresh_filter_options(self, bubbles: List[dict]):
        current_page = self._page_filter.currentData() if hasattr(self, "_page_filter") else None
        current_character = self._character_filter.currentData() if hasattr(self, "_character_filter") else None
        current_character_text = self._character_filter.currentText().strip() if hasattr(self, "_character_filter") else ""

        pages = set()
        characters = set()
        total_pages = 0
        cv = self._cv()
        if cv is not None and hasattr(cv, "images"):
            try:
                total_pages = len(getattr(cv, "images", []) or [])
            except Exception:
                total_pages = 0
        if total_pages <= 0:
            try:
                total_pages = len(getattr(self.project, "images", []) or [])
            except Exception:
                total_pages = 0

        for rec in bubbles:
            try:
                if rec.get("img_idx") is not None:
                    pages.add(int(rec.get("img_idx")))
            except Exception:
                pass
            name = str(rec.get("character_name", "") or "").strip()
            if name:
                characters.add(name)

        self._page_filter.blockSignals(True)
        self._page_filter.clear()
        self._page_filter.addItem("Все страницы", None)
        if total_pages > 0:
            page_indices = range(total_pages)
        else:
            page_indices = sorted(pages)
        for idx in page_indices:
            self._page_filter.addItem(f"Страница #{idx + 1}", idx)
        page_idx = self._page_filter.findData(current_page)
        self._page_filter.setCurrentIndex(page_idx if page_idx >= 0 else 0)
        self._page_filter.blockSignals(False)

        self._character_filter.blockSignals(True)
        self._character_filter.clear()
        self._character_filter.addItem("Все персонажи", None)
        for name in sorted(characters, key=lambda x: x.casefold()):
            self._character_filter.addItem(name, name)
        char_idx = self._character_filter.findData(current_character)
        if char_idx >= 0:
            self._character_filter.setCurrentIndex(char_idx)
        else:
            self._character_filter.setCurrentIndex(0)
            if current_character_text and current_character_text != "Все персонажи":
                self._character_filter.setEditText(current_character_text)
        self._character_filter.blockSignals(False)

    def _filter_bubbles(self, bubbles: List[dict]) -> List[dict]:
        return [rec for rec in bubbles if self._matches_applied_filters(rec)]

    def _matches_applied_filters(self, rec: dict) -> bool:
        page = self._applied_filters.get("page")
        character = self._applied_filters.get("character")
        scope = self._applied_filters.get("scope", "all")
        query = str(self._applied_filters.get("query", "") or "").casefold()

        if page is not None:
            try:
                if int(rec.get("img_idx")) != int(page):
                    return False
            except Exception:
                return False

        if character is not None:
            rec_character = str(rec.get("character_name", "") or "")
            if rec_character.casefold() != str(character).casefold():
                return False

        if not query:
            return True

        original_text = str(rec.get("original_text", "") or "")
        translated_text = str(rec.get("text", "") or "")
        if scope == "original":
            haystack = original_text
        elif scope == "translation":
            haystack = translated_text
        else:
            haystack = f"{original_text}\n{translated_text}"
        return query in haystack.casefold()

    # Вспомогательные методы
    def _cv(self):
        cv = getattr(self, "canvas", None)
        if cv is None or _is_deleted(cv):
            return None
        return cv

    def _flush_panel_text_updates(self):
        """Применяет накопленные изменения текста пузырей к модели (debounced batch update)."""
        if not self.model:
            self._panel_pending_updates.clear()
            return
        cv = self._cv()
        for bid, text in list(self._panel_pending_updates.items()):
            b = cv.bubbles.get(bid) if cv else None
            if b:
                rec = {'id': bid, 'text': text,
                       'img_idx': b.img_idx, 'img_u': b.img_u, 'img_v': b.img_v, 'side': b.side}
                self.model.update(rec, cv.uid if cv else "bubbles_panel")
        self._panel_pending_updates.clear()
