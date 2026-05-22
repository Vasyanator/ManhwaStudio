# terms_tab_qt.py
# Переписано с Tkinter на PyQt6. Использование:
#   self.terms_tab = TermsTab(self.project)
#   tabs.addTab(self.terms_tab, "Термины")
#
# Требования к проекту:
#   - У объекта `project` должен быть атрибут `terms_file` — путь к JSON-файлу с терминами.
#     Папка для файла будет создана при необходимости.
#
# ВНИМАНИЕ: Любая логика обновления notes_file удалена — используется только project.terms_file.
#
# Зависимости: PyQt6 (pip install PyQt6)

from __future__ import annotations

import json
import os
from typing import List

from PyQt6 import QtCore, QtGui, QtWidgets


class TermsTab(QtWidgets.QWidget):
    TAB_TITLE = "Термины"
    CARD_WIDTH_PX = 520

    def __init__(self, project, parent=None):
        super().__init__(parent)
        self.project = project

        # Гарантируем наличие папки под JSON
        terms_path = self._json_path()
        terms_dir = os.path.dirname(terms_path)
        if terms_dir:
            os.makedirs(terms_dir, exist_ok=True)

        self._build_ui()
        self._apply_dark_palette()
        self._rebuild_tag_filter_options()
        self.refresh_list()

    # ---------------- UI ----------------

    def _build_ui(self):
        root = QtWidgets.QVBoxLayout(self)
        root.setContentsMargins(8, 8, 8, 8)
        root.setSpacing(8)

        # Панель фильтров
        bar = QtWidgets.QHBoxLayout()
        bar.setSpacing(8)

        bar.addWidget(QtWidgets.QLabel("Поиск:"))
        self.search_edit = QtWidgets.QLineEdit()
        self.search_edit.setPlaceholderText("название, оригинал, теги или текст…")
        self.search_edit.setClearButtonEnabled(True)
        self.search_edit.setFixedWidth(320)
        self.search_edit.textChanged.connect(self.refresh_list)
        bar.addWidget(self.search_edit)

        bar.addSpacing(8)
        bar.addWidget(QtWidgets.QLabel("Тег:"))
        self.tag_combo = QtWidgets.QComboBox()
        self.tag_combo.currentIndexChanged.connect(self.refresh_list)
        self.tag_combo.setMinimumWidth(200)
        bar.addWidget(self.tag_combo)

        reset_btn = QtWidgets.QPushButton("Сбросить")
        reset_btn.clicked.connect(self._reset_filters)
        bar.addWidget(reset_btn)

        bar.addStretch(1)
        root.addLayout(bar)

        # Прокручиваемая область со списком карточек
        self.scroll_area = QtWidgets.QScrollArea()
        self.scroll_area.setWidgetResizable(True)

        self.list_host = QtWidgets.QWidget()
        self.scroll_area.setWidget(self.list_host)

        self.list_layout = QtWidgets.QVBoxLayout(self.list_host)
        self.list_layout.setContentsMargins(0, 0, 0, 0)
        self.list_layout.setSpacing(8)
        self.list_layout.addStretch(1)

        root.addWidget(self.scroll_area, 1)

        # Нижняя панель с кнопкой "Добавить"
        bottom = QtWidgets.QHBoxLayout()
        bottom.addStretch(1)
        add_btn = QtWidgets.QPushButton("Добавить")
        add_btn.clicked.connect(self._on_add)
        bottom.addWidget(add_btn)
        root.addLayout(bottom)

        # Стиль карточек
        self.setStyleSheet("""
            QWidget { font-size: 12px; }
            QLineEdit, QTextEdit, QComboBox {
                background: #1e1e1e; color: #e6e6e6; border: 1px solid #3a3a3a;
            }
            QPushButton { background: #2d2d2d; color: #e6e6e6; border: 1px solid #3a3a3a; padding: 6px 10px; }
            QPushButton:hover { background: #353535; }
            QFrame#Card { background: #1f1f1f; border: 1px solid #3a3a3a; border-radius: 8px; }
            QLabel#CardName { font-weight: 700; font-size: 14px; }
            QLabel#CardMeta { color: #9aa0a6; font-style: italic; }
            QLabel#CardText { color: #e6e6e6; }
        """)

    def _apply_dark_palette(self):
        pal = self.palette()
        pal.setColor(QtGui.QPalette.ColorRole.Window, QtGui.QColor(30, 30, 30))
        pal.setColor(QtGui.QPalette.ColorRole.Base, QtGui.QColor(24, 24, 24))
        pal.setColor(QtGui.QPalette.ColorRole.AlternateBase, QtGui.QColor(36, 36, 36))
        pal.setColor(QtGui.QPalette.ColorRole.Text, QtGui.QColor(230, 230, 230))
        pal.setColor(QtGui.QPalette.ColorRole.WindowText, QtGui.QColor(230, 230, 230))
        pal.setColor(QtGui.QPalette.ColorRole.Button, QtGui.QColor(45, 45, 45))
        pal.setColor(QtGui.QPalette.ColorRole.ButtonText, QtGui.QColor(230, 230, 230))
        pal.setColor(QtGui.QPalette.ColorRole.Highlight, QtGui.QColor(53, 132, 228))
        pal.setColor(QtGui.QPalette.ColorRole.HighlightedText, QtGui.QColor(255, 255, 255))
        self.setPalette(pal)

    def _reset_filters(self):
        self.search_edit.clear()
        idx = self.tag_combo.findText("(все)")
        self.tag_combo.setCurrentIndex(idx if idx >= 0 else 0)

    # ---------------- Данные ----------------

    def _json_path(self) -> str:
        path = getattr(self.project, "terms_file", None)
        if not path:
            raise RuntimeError("project.terms_file не задан.")
        return path

    def _load_db(self) -> List[dict]:
        """
        Формат JSON:
            [{"name": str, "orig_name": str, "description": str, "tags": [str, ...]}, ...]
        """
        path = self._json_path()
        if not os.path.isfile(path):
            return []
        try:
            with open(path, "r", encoding="utf-8") as f:
                data = json.load(f) or []
        except Exception:
            QtWidgets.QMessageBox.warning(self, "Термины", "Не удалось прочитать JSON с терминами. Файл будет проигнорирован.")
            return []

        db: List[dict] = []
        for it in data:
            name = (it.get("name") or "").strip()
            if not name:
                continue
            orig = (it.get("orig_name") or "").strip()
            desc = it.get("description") or ""
            tags = it.get("tags") or []
            if isinstance(tags, str):
                tags = [tags]
            db.append({
                "name": name,
                "orig_name": orig,
                "description": desc,
                "tags": [t for t in (tags or []) if (t or "").strip()]
            })
        db.sort(key=lambda x: x["name"].lower())
        return db

    def _save_db(self, db: List[dict]):
        path = self._json_path()
        tmp = path + ".tmp"
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(db, f, ensure_ascii=False, indent=2)
        if os.path.exists(path):
            os.replace(tmp, path)
        else:
            os.rename(tmp, path)

    def _db_find_index(self, db: List[dict], name: str) -> int:
        for i, it in enumerate(db):
            if it["name"] == name:
                return i
        return -1

    def _collect_tags(self) -> List[str]:
        tags: set[str] = set()
        for it in self._load_db():
            for t in it.get("tags", []):
                t = (t or "").strip()
                if t:
                    tags.add(t)
        return sorted(tags, key=lambda s: s.lower())

    def _rebuild_tag_filter_options(self):
        keep = self.tag_combo.currentText() if self.tag_combo.count() else "(все)"
        values = ["(все)"] + self._collect_tags()
        self.tag_combo.clear()
        self.tag_combo.addItems(values)
        if keep in values:
            self.tag_combo.setCurrentText(keep)
        else:
            self.tag_combo.setCurrentIndex(0)

    # ---------------- Отрисовка списка ----------------

    def refresh_list(self):
        # очистить карточки, оставить завершающий stretch
        while self.list_layout.count() > 1:
            item = self.list_layout.takeAt(0)
            w = item.widget()
            if w is not None:
                w.deleteLater()

        db = self._load_db()
        term = (self.search_edit.text() or "").strip().lower()
        selected_tag = (self.tag_combo.currentText() or "(все)").strip().lower()

        def match(item: dict) -> bool:
            name = item.get("name", "")
            orig = item.get("orig_name", "")
            desc = item.get("description", "")
            tags = [(t or "").strip() for t in (item.get("tags") or []) if (t or "").strip()]

            hay = " ".join([name, orig, desc, ", ".join(tags)]).lower()
            by_term = (term in hay) if term else True

            if selected_tag == "(все)":
                by_tag = True
            else:
                tags_norm = [t.lower() for t in tags]
                by_tag = selected_tag in tags_norm

            return by_term and by_tag

        filtered = [it for it in db if match(it)]

        if not filtered:
            self.list_layout.insertWidget(0, QtWidgets.QLabel("Ничего не найдено."))
            return

        for it in filtered:
            self._create_card(
                name=it.get("name", ""),
                orig_name=it.get("orig_name", ""),
                description=it.get("description", ""),
                tags=it.get("tags", []),
            )

    def _create_card(self, name: str, orig_name: str, description: str, tags: List[str]):
        card = QtWidgets.QFrame()
        card.setObjectName("Card")

        outer = QtWidgets.QVBoxLayout(card)
        outer.setContentsMargins(10, 10, 10, 10)
        outer.setSpacing(6)

        # Верхняя строка: название + кнопки
        top = QtWidgets.QHBoxLayout()
        title = QtWidgets.QLabel(name)
        title.setObjectName("CardName")
        top.addWidget(title, 1)

        edit_btn = QtWidgets.QPushButton("Редактировать")
        edit_btn.clicked.connect(lambda: self._on_edit(name))
        top.addWidget(edit_btn)

        del_btn = QtWidgets.QPushButton("Удалить")
        del_btn.clicked.connect(lambda: self._delete_term(name))
        top.addWidget(del_btn)

        outer.addLayout(top)

        # Оригинальное название
        meta_orig = f"Оригинальное название: {orig_name.strip() or '—'}"
        orig_lbl = QtWidgets.QLabel(meta_orig)
        orig_lbl.setObjectName("CardMeta")
        outer.addWidget(orig_lbl)

        # Теги
        if tags:
            meta_tags = QtWidgets.QLabel(f"Теги: {', '.join(t for t in tags if t)}")
            meta_tags.setObjectName("CardMeta")
            outer.addWidget(meta_tags)

        # Описание
        desc_lbl = QtWidgets.QLabel(description or "")
        desc_lbl.setObjectName("CardText")
        desc_lbl.setWordWrap(True)
        desc_lbl.setFixedWidth(self.CARD_WIDTH_PX)
        outer.addWidget(desc_lbl)

        self.list_layout.insertWidget(self.list_layout.count() - 1, card)

    # ---------------- Команды ----------------

    def _on_add(self):
        dlg = EditTermDialog(
            parent=self,
            title="Добавить термин",
            available_tags=self._collect_tags(),
            initial_tags=[],
        )
        if dlg.exec() == QtWidgets.QDialog.DialogCode.Accepted:
            self._create_new(
                name=dlg.name(),
                orig_name=dlg.orig_name(),
                desc=dlg.description(),
                tags=dlg.tags(),
            )

    def _on_edit(self, name: str):
        db = self._load_db()
        i = self._db_find_index(db, name)
        if i < 0:
            return
        it = db[i]
        dlg = EditTermDialog(
            parent=self,
            title=f"Редактировать: {name}",
            initial_name=it.get("name", ""),
            initial_orig_name=it.get("orig_name", ""),
            initial_desc=it.get("description", ""),
            available_tags=self._collect_tags(),
            initial_tags=it.get("tags", []),
            deletable=True,
        )
        code = dlg.exec()
        if code == QtWidgets.QDialog.DialogCode.Accepted:
            self._update_existing(
                old_name=name,
                new_name=dlg.name(),
                orig_name=dlg.orig_name(),
                desc=dlg.description(),
                tags=dlg.tags(),
            )
        elif code == EditTermDialog.DeleteCode:
            self._delete_term(name)

    def _create_new(self, name: str, orig_name: str, desc: str, tags: List[str]):
        name = (name or "").strip()
        if not name:
            QtWidgets.QMessageBox.critical(self, "Ошибка", "Название не может быть пустым.")
            return

        tags = sorted({(t or "").strip() for t in (tags or []) if (t or "").strip()}, key=str.lower)

        db = self._load_db()
        i = self._db_find_index(db, name)
        if i >= 0:
            res = QtWidgets.QMessageBox.question(
                self, "Подтверждение", f"«{name}» уже существует. Перезаписать?",
                QtWidgets.QMessageBox.StandardButton.Yes | QtWidgets.QMessageBox.StandardButton.No
            )
            if res != QtWidgets.QMessageBox.StandardButton.Yes:
                return
            db[i]["orig_name"] = orig_name or ""
            db[i]["description"] = desc or ""
            db[i]["tags"] = tags
        else:
            db.append({"name": name, "orig_name": orig_name or "", "description": desc or "", "tags": tags})
            db.sort(key=lambda x: x["name"].lower())

        try:
            self._save_db(db)
        except Exception as e:
            QtWidgets.QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить термин в JSON:\n{e}")
            return

        self._rebuild_tag_filter_options()
        self.refresh_list()

    def _update_existing(self, old_name: str, new_name: str, orig_name: str, desc: str, tags: List[str]):
        new_name = (new_name or "").strip()
        if not new_name:
            QtWidgets.QMessageBox.critical(self, "Ошибка", "Название не может быть пустым.")
            return

        tags = sorted({(t or "").strip() for t in (tags or []) if (t or "").strip()}, key=str.lower)

        db = self._load_db()
        i_old = self._db_find_index(db, old_name)
        if i_old < 0:
            return self._create_new(new_name, orig_name, desc, tags)

        if new_name != old_name:
            i_new = self._db_find_index(db, new_name)
            if i_new >= 0:
                res = QtWidgets.QMessageBox.question(
                    self, "Подтверждение", f"«{new_name}» уже существует. Перезаписать?",
                    QtWidgets.QMessageBox.StandardButton.Yes | QtWidgets.QMessageBox.StandardButton.No
                )
                if res != QtWidgets.QMessageBox.StandardButton.Yes:
                    return

        rec = db[i_old]
        rec["name"] = new_name
        rec["orig_name"] = orig_name or ""
        rec["description"] = desc or ""
        rec["tags"] = tags

        # Удаляем возможные дубликаты по имени, оставляем последний
        seen = {}
        for it in db:
            seen[it["name"]] = it
        db = sorted(seen.values(), key=lambda x: x["name"].lower())

        try:
            self._save_db(db)
        except Exception as e:
            QtWidgets.QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить изменения:\n{e}")
            return

        self._rebuild_tag_filter_options()
        self.refresh_list()

    def _delete_term(self, name: str):
        if not name:
            return
        res = QtWidgets.QMessageBox.question(
            self, "Удалить термин", f"Точно удалить «{name}»? Это действие нельзя отменить.",
            QtWidgets.QMessageBox.StandardButton.Yes | QtWidgets.QMessageBox.StandardButton.No
        )
        if res != QtWidgets.QMessageBox.StandardButton.Yes:
            return

        db = self._load_db()
        i = self._db_find_index(db, name)
        if i >= 0:
            del db[i]
            try:
                self._save_db(db)
            except Exception as e:
                QtWidgets.QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить изменения:\n{e}")
                return

        self._rebuild_tag_filter_options()
        self.refresh_list()


class EditTermDialog(QtWidgets.QDialog):
    """
    Диалог добавления/редактирования термина.
    Accept -> сохранить, Reject -> отмена.
    Доп. код DeleteCode для "Удалить".
    """
    DeleteCode = 10

    def __init__(
        self,
        parent=None,
        title: str = "Термин",
        initial_name: str = "",
        initial_orig_name: str = "",
        initial_desc: str = "",
        available_tags: List[str] | None = None,
        initial_tags: List[str] | None = None,
        deletable: bool = False,
    ):
        super().__init__(parent)
        self.setWindowTitle(title)
        self.setModal(True)
        self.resize(560, 560)

        self._available_tags = sorted(set(available_tags or []), key=str.lower)
        self._selected_tags = list(initial_tags or [])

        v = QtWidgets.QVBoxLayout(self)
        v.setContentsMargins(12, 12, 12, 12)
        v.setSpacing(10)

        form = QtWidgets.QFormLayout()
        form.setLabelAlignment(QtCore.Qt.AlignmentFlag.AlignLeft)

        self.name_edit = QtWidgets.QLineEdit(initial_name or "")
        form.addRow("Название:", self.name_edit)

        self.orig_edit = QtWidgets.QLineEdit(initial_orig_name or "")
        form.addRow("Оригинальное название:", self.orig_edit)

        self.desc_edit = QtWidgets.QTextEdit()
        self.desc_edit.setAcceptRichText(False)
        self.desc_edit.setPlainText(initial_desc or "")
        self.desc_edit.setMinimumHeight(160)
        form.addRow("Описание:", self.desc_edit)

        v.addLayout(form)

        # Теги
        grp = QtWidgets.QGroupBox("Теги")
        gv = QtWidgets.QVBoxLayout(grp)

        top_row = QtWidgets.QHBoxLayout()
        top_row.addWidget(QtWidgets.QLabel("Выбрать или ввести:"))
        self.tag_combo = QtWidgets.QComboBox()
        self.tag_combo.setEditable(True)
        self.tag_combo.addItems(self._available_tags)
        top_row.addWidget(self.tag_combo, 1)
        add_btn = QtWidgets.QPushButton("Добавить")
        add_btn.clicked.connect(self._on_tag_add)
        top_row.addWidget(add_btn)
        gv.addLayout(top_row)

        list_row = QtWidgets.QHBoxLayout()
        self.tag_list = QtWidgets.QListWidget()
        self.tag_list.setSelectionMode(QtWidgets.QAbstractItemView.SelectionMode.ExtendedSelection)
        for t in self._selected_tags:
            self.tag_list.addItem(t)
        list_row.addWidget(self.tag_list, 1)

        btn_col = QtWidgets.QVBoxLayout()
        del_sel = QtWidgets.QPushButton("Удалить выбранные")
        del_sel.clicked.connect(self._on_tag_remove_selected)
        btn_col.addWidget(del_sel)
        clr = QtWidgets.QPushButton("Очистить")
        clr.clicked.connect(self._on_tag_clear)
        btn_col.addWidget(clr)
        btn_col.addStretch(1)
        list_row.addLayout(btn_col)
        gv.addLayout(list_row)

        v.addWidget(grp)

        # Кнопки управления
        btns = QtWidgets.QHBoxLayout()
        if deletable:
            del_btn = QtWidgets.QPushButton("Удалить")
            del_btn.clicked.connect(self._on_delete_clicked)
            btns.addWidget(del_btn)

        btns.addStretch(1)
        cancel_btn = QtWidgets.QPushButton("Отмена")
        cancel_btn.clicked.connect(self.reject)
        btns.addWidget(cancel_btn)

        save_btn = QtWidgets.QPushButton("Сохранить")
        save_btn.setDefault(True)
        save_btn.clicked.connect(self.accept)
        btns.addWidget(save_btn)
        v.addLayout(btns)

        # Шорткаты
        QtGui.QShortcut(QtGui.QKeySequence("Escape"), self, activated=self.reject)
        QtGui.QShortcut(QtGui.QKeySequence("Ctrl+Return"), self, activated=self.accept)

        self.name_edit.setFocus()

    # --- Публичные геттеры ---

    def name(self) -> str:
        return (self.name_edit.text() or "").strip()

    def orig_name(self) -> str:
        return (self.orig_edit.text() or "").strip()

    def description(self) -> str:
        return (self.desc_edit.toPlainText() or "").strip()

    def tags(self) -> List[str]:
        return [self.tag_list.item(i).text() for i in range(self.tag_list.count())]

    # --- Теги ---

    def _on_tag_add(self):
        t = (self.tag_combo.currentText() or "").strip()
        if not t:
            return
        existing = {self.tag_list.item(i).text() for i in range(self.tag_list.count())}
        if t not in existing:
            self.tag_list.addItem(t)
        self.tag_combo.setCurrentText("")

    def _on_tag_remove_selected(self):
        for item in self.tag_list.selectedItems():
            row = self.tag_list.row(item)
            self.tag_list.takeItem(row)

    def _on_tag_clear(self):
        self.tag_list.clear()

    # --- Удаление ---

    DeleteCode = 10

    def _on_delete_clicked(self):
        self.done(EditTermDialog.DeleteCode)
