from __future__ import annotations
import traceback
# characters_tab_qt.py
# Переписано с Tkinter на PyQt6. Использование:
#   self.charas_tab = CharactersTab(self.project)
#   tabs.addTab(self.charas_tab, "Персонажи")
#
# Требования к проекту:
#   - У объекта `project` должен быть атрибут `char_dir` (папка с данными персонажей).
#
# Зависимости: PyQt6 (pip install PyQt6)



import json
import os
import shutil
import string
from typing import List

from PyQt6 import QtCore, QtGui, QtWidgets


class CharactersTab(QtWidgets.QWidget):
    """
    Вкладка со списком персонажей (чтение/поиск/фильтрация/редактирование).
    Хранение: characters.json + {name}.png в project.char_dir.
    """

    CARD_IMAGE_SIZE = QtCore.QSize(192, 192)
    CARD_WIDTH_PX = 820

    def __init__(self, project, parent=None):
        super().__init__(parent)
        self.project = project
        os.makedirs(self.project.char_dir, exist_ok=True)

        self._thumb_cache: dict[str, QtGui.QPixmap] = {}

        self._build_ui()
        self._apply_dark_palette()  # Тёмная тема для вкладки
        self._rebuild_group_filter_options()
        self.refresh_list()

    # ---------------- UI ----------------
    def _build_ui(self):
        root = QtWidgets.QVBoxLayout(self)
        root.setContentsMargins(8, 8, 8, 8)
        root.setSpacing(8)

        # Верхняя панель: поиск + фильтр по группе + сброс
        bar = QtWidgets.QHBoxLayout()
        bar.setSpacing(8)

        bar.addWidget(QtWidgets.QLabel("Поиск:"))
        self.search_edit = QtWidgets.QLineEdit()
        self.search_edit.setPlaceholderText("имя, группы или текст описания…")
        self.search_edit.textChanged.connect(self.refresh_list)
        self.search_edit.setClearButtonEnabled(True)
        self.search_edit.setFixedWidth(320)
        bar.addWidget(self.search_edit)

        bar.addSpacing(8)
        bar.addWidget(QtWidgets.QLabel("Группа:"))
        self.group_combo = QtWidgets.QComboBox()
        self.group_combo.currentIndexChanged.connect(self.refresh_list)
        self.group_combo.setMinimumWidth(200)
        bar.addWidget(self.group_combo)

        reset_btn = QtWidgets.QPushButton("Сбросить")
        reset_btn.clicked.connect(self._reset_filters)
        bar.addWidget(reset_btn)

        bar.addStretch(1)

        root.addLayout(bar)

        # Прокручиваемая область с карточками
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

    def _apply_dark_palette(self):
        # Локальная тёмная палитра (не трогаем весь app глобально)
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

        # Небольшой стиль для "карточек"
        self.setStyleSheet("""
            QWidget { font-size: 12px; }
            QLineEdit, QTextEdit, QComboBox, QListView, QListWidget, QSpinBox {
                background: #1e1e1e; color: #e6e6e6; border: 1px solid #3a3a3a;
            }
            QPushButton { background: #2d2d2d; color: #e6e6e6; border: 1px solid #3a3a3a; padding: 6px 10px; }
            QPushButton:hover { background: #353535; }
            QFrame#Card {
                background: #1f1f1f;
                border: 1px solid #3a3a3a;
                border-radius: 8px;
            }
            QLabel#CardName { font-weight: 700; font-size: 14px; }
            QLabel#CardMeta { color: #9aa0a6; font-style: italic; }
        """)

    # ---------------- Данные/фильтры ----------------

    def _reset_filters(self):
        self.search_edit.clear()
        # установить "(все)" если есть
        idx = self.group_combo.findText("(все)")
        self.group_combo.setCurrentIndex(idx if idx >= 0 else 0)

    def _collect_groups(self) -> List[str]:
        groups: set[str] = set()
        for it in self._load_db():
            for g in it.get("group", []):
                g = (g or "").strip()
                if g:
                    groups.add(g)
        return sorted(groups, key=lambda s: s.lower())

    def _rebuild_group_filter_options(self):
        keep = self.group_combo.currentText() if self.group_combo.count() else "(все)"
        values = ["(все)"] + self._collect_groups()
        self.group_combo.clear()
        self.group_combo.addItems(values)
        if keep in values:
            self.group_combo.setCurrentText(keep)
        else:
            self.group_combo.setCurrentIndex(0)

    # ---------------- Отрисовка списка ----------------

    def refresh_list(self):
        # очистить карточки, оставить завершающий stretch
        while self.list_layout.count() > 1:
            item = self.list_layout.takeAt(0)
            w = item.widget()
            if w is not None:
                w.deleteLater()
        self._thumb_cache.clear()

        db = self._load_db()

        term = (self.search_edit.text() or "").strip().lower()
        selected_group = (self.group_combo.currentText() or "(все)").strip().lower()

        def match(item: dict) -> bool:
            name = item.get("name") or ""
            desc = item.get("description") or ""
            groups = [(g or "").strip() for g in (item.get("group") or []) if (g or "").strip()]
            hay = " ".join([name, desc, ", ".join(groups)]).lower()
            by_term = (term in hay) if term else True
            if selected_group == "(все)":
                by_group = True
            else:
                by_group = selected_group in [g.lower() for g in groups]
            return by_term and by_group

        filtered = [it for it in db if match(it)]

        if not filtered:
            lbl = QtWidgets.QLabel("Ничего не найдено.")
            self.list_layout.insertWidget(0, lbl)
            return

        for idx, it in enumerate(filtered):
            self._create_card(it.get("name", ""), (it.get("description") or "").strip(), it.get("group", []))

    def _create_card(self, name: str, description: str, groups: List[str]):
        card_frame = QtWidgets.QFrame()
        card_frame.setObjectName("Card")

        outer = QtWidgets.QHBoxLayout(card_frame)
        outer.setContentsMargins(10, 10, 10, 10)
        outer.setSpacing(12)

        # Левая колонка: картинка + "Редактировать"
        left = QtWidgets.QVBoxLayout()
        left.setSpacing(2)

        img_lbl = QtWidgets.QLabel()
        img_lbl.setFixedSize(self.CARD_IMAGE_SIZE)
        img_lbl.setAlignment(QtCore.Qt.AlignmentFlag.AlignCenter)
        pix = self._load_image_pixmap(name)
        if pix:
            img_lbl.setPixmap(pix.scaled(self.CARD_IMAGE_SIZE, QtCore.Qt.AspectRatioMode.KeepAspectRatio,
                                         QtCore.Qt.TransformationMode.SmoothTransformation))
        else:
            img_lbl.setText("Нет\nизображения")

        left.addWidget(img_lbl)

        edit_btn = QtWidgets.QPushButton("Редактировать")
        edit_btn.clicked.connect(lambda: self._on_edit(name))
        left.addWidget(edit_btn)

        left.addStretch(1)
        outer.addLayout(left)

        # Правая колонка: имя, группы, описание
        right = QtWidgets.QVBoxLayout()
        right.setSpacing(6)

        name_lbl = QtWidgets.QLabel(name)
        name_lbl.setObjectName("CardName")
        right.addWidget(name_lbl)

        if groups:
            meta_lbl = QtWidgets.QLabel(f"Группы: {', '.join(g for g in groups if g)}")
            meta_lbl.setObjectName("CardMeta")
            right.addWidget(meta_lbl)

        desc_lbl = QtWidgets.QLabel(description or "")
        desc_lbl.setWordWrap(True)
        desc_lbl.setFixedWidth(self.CARD_WIDTH_PX)
        right.addWidget(desc_lbl)

        outer.addLayout(right)
        self.list_layout.insertWidget(self.list_layout.count() - 1, card_frame)

    # ---------------- Хранение ----------------

    def _json_path(self) -> str:
        return os.path.join(self.project.char_dir, "characters.json")

    def _txt_path(self, name: str) -> str:
        return os.path.join(self.project.char_dir, f"{name}.txt")

    def _image_path(self, name: str) -> str:
        return os.path.join(self.project.char_dir, f"{name}.png")

    def _safe_name(self, name: str) -> str:
        bad = set('<>:"/\\|?*\n\r\t')
        cleaned = "".join(c for c in name.strip() if c not in bad)
        allowed = string.printable + "абвгдеёжзийклмнопрстуфхцчшщьыъэюяАБВГДЕЁЖЗИЙКЛМНОПРСТУФХЦЧШЩЬЫЪЭЮЯ _-"
        cleaned = "".join(c for c in cleaned if c in allowed)
        return cleaned[:64] if cleaned else "unnamed"

    def _load_db(self) -> List[dict]:
        """
        [{"name": str, "description": str, "group": [str, ...]}, ...]
        При отсутствии/битом JSON — мигрирует *.txt -> json (удаляет .txt).
        """
        path = self._json_path()
        db: List[dict] = []

        if os.path.isfile(path):
            try:
                with open(path, "r", encoding="utf-8") as f:
                    data = json.load(f) or []
                for it in data:
                    name = (it.get("name") or "").strip()
                    desc = it.get("description") or ""
                    group = it.get("group") or []
                    if isinstance(group, str):
                        group = [group]
                    if name:
                        db.append({"name": name, "description": desc, "group": list(group)})
                db.sort(key=lambda x: x["name"].lower())
                return db
            except Exception:
                traceback.print_exc()
                pass  # попробуем миграцию

        # Миграция из .txt
        names = []
        for fn in os.listdir(self.project.char_dir):
            if fn.lower().endswith(".txt"):
                names.append(os.path.splitext(fn)[0])
        names.sort(key=lambda s: s.lower())

        for nm in names:
            try:
                with open(self._txt_path(nm), "r", encoding="utf-8") as f:
                    desc = f.read().strip()
            except Exception:
                desc = ""
            db.append({"name": nm, "description": desc, "group": []})

        self._save_db(db)

        # удалить .txt
        for nm in names:
            try:
                os.remove(self._txt_path(nm))
            except Exception:
                pass

        return db

    def _save_db(self, db: List[dict]):
        path = self._json_path()
        tmp = path + ".tmp"
        os.makedirs(self.project.char_dir, exist_ok=True)
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

    # ---------------- Изображения ----------------

    def _load_image_pixmap(self, name: str) -> QtGui.QPixmap | None:
        if name in self._thumb_cache:
            return self._thumb_cache[name]
        p = self._image_path(name)
        if not os.path.isfile(p):
            return None
        pix = QtGui.QPixmap(p)
        if pix.isNull():
            return None
        self._thumb_cache[name] = pix
        return pix

    # ---------------- Команды ----------------

    def _on_add(self):
        dlg = EditCharacterDialog(
            parent=self,
            title="Добавить персонажа",
            available_groups=self._collect_groups(),
            initial_groups=[]
        )
        if dlg.exec() == QtWidgets.QDialog.DialogCode.Accepted:
            name = self._safe_name(dlg.name())
            if not name:
                return
            desc = dlg.description()
            groups = self._normalize_groups(dlg.groups())
            image = dlg.image()

            db = self._load_db()
            i = self._db_find_index(db, name)
            if i >= 0:
                res = QtWidgets.QMessageBox.question(
                    self, "Подтверждение", f"«{name}» уже существует. Перезаписать?",
                    QtWidgets.QMessageBox.StandardButton.Yes | QtWidgets.QMessageBox.StandardButton.No
                )
                if res != QtWidgets.QMessageBox.StandardButton.Yes:
                    return
                db[i]["description"] = desc
                db[i]["group"] = groups
            else:
                db.append({"name": name, "description": desc, "group": groups})
                db.sort(key=lambda x: x["name"].lower())

            try:
                self._save_db(db)
            except Exception as e:
                QtWidgets.QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить JSON:\n{e}")
                return

            if image is not None and not image.isNull():
                image.save(self._image_path(name), "PNG")

            self._rebuild_group_filter_options()
            self.refresh_list()

    def _on_edit(self, name: str):
        # найти текущие данные
        db = self._load_db()
        i = self._db_find_index(db, name)
        if i < 0:
            return
        item = db[i]
        current_desc = item.get("description", "")
        init_groups = item.get("group", [])
        img_path = self._image_path(name)
        dlg = EditCharacterDialog(
            parent=self,
            title=f"Редактировать: {name}",
            initial_name=name,
            initial_desc=current_desc,
            initial_image_path=img_path if os.path.isfile(img_path) else None,
            available_groups=self._collect_groups(),
            initial_groups=init_groups,
            deletable=True
        )
        code = dlg.exec()

        if code == QtWidgets.QDialog.DialogCode.Accepted:
            new_name = self._safe_name(dlg.name())
            if not new_name:
                QtWidgets.QMessageBox.warning(self, "Ошибка", "Имя не может быть пустым.")
                return
            desc = dlg.description()
            groups = self._normalize_groups(dlg.groups())
            image = dlg.image()  # QImage | None

            # если новое имя занято, спросим
            if new_name != name:
                j = self._db_find_index(db, new_name)
                if j >= 0:
                    res = QtWidgets.QMessageBox.question(
                        self, "Подтверждение", f"«{new_name}» уже существует. Перезаписать?",
                        QtWidgets.QMessageBox.StandardButton.Yes | QtWidgets.QMessageBox.StandardButton.No
                    )
                    if res != QtWidgets.QMessageBox.StandardButton.Yes:
                        return

            # обновить запись
            item["name"] = new_name
            item["description"] = desc
            item["group"] = groups

            # удалить дубликаты, если возникли
            seen = {}
            for it in db:
                seen[it["name"]] = it  # последний побеждает
            db = sorted(seen.values(), key=lambda x: x["name"].lower())

            try:
                self._save_db(db)
            except Exception as e:
                QtWidgets.QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить JSON:\n{e}")
                return

            # переименовать/обновить картинку
            old_png, new_png = self._image_path(name), self._image_path(new_name)
            try:
                if new_name != name and os.path.exists(old_png):
                    if os.path.exists(new_png):
                        os.remove(new_png)
                    shutil.move(old_png, new_png)
            except Exception:
                QtWidgets.QMessageBox.warning(self, "Внимание", "Изображение не удалось переименовать.")

            if image is not None:
                image.save(new_png, "PNG")

            self._rebuild_group_filter_options()
            self.refresh_list()

        elif code == EditCharacterDialog.DeleteCode:
            # удаление персонажа
            res = QtWidgets.QMessageBox.question(
                self, "Удалить персонажа",
                f"Точно удалить «{name}»? Это действие нельзя отменить.",
                QtWidgets.QMessageBox.StandardButton.Yes | QtWidgets.QMessageBox.StandardButton.No
            )
            if res == QtWidgets.QMessageBox.StandardButton.Yes:
                # удалить запись
                db = self._load_db()
                i = self._db_find_index(db, name)
                if i >= 0:
                    del db[i]
                    try:
                        self._save_db(db)
                    except Exception as e:
                        QtWidgets.QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить JSON:\n{e}")
                        return
                # удалить картинку
                try:
                    p = self._image_path(name)
                    if os.path.exists(p):
                        os.remove(p)
                except Exception:
                    pass
                self._rebuild_group_filter_options()
                self.refresh_list()

    @staticmethod
    def _normalize_groups(groups: List[str]) -> List[str]:
        return sorted({(g or "").strip() for g in (groups or []) if (g or "").strip()}, key=str.lower)


class EditCharacterDialog(QtWidgets.QDialog):
    """
    Диалог добавления/редактирования персонажа.
    Accept -> сохранить, Reject -> отмена.
    Доп. код DeleteCode для "Удалить".
    """
    DeleteCode = 10

    PREVIEW_SIZE = QtCore.QSize(320, 320)

    def __init__(
        self,
        parent=None,
        title: str = "Персонаж",
        initial_name: str = "",
        initial_desc: str = "",
        initial_image_path: str | None = None,
        available_groups: List[str] | None = None,
        initial_groups: List[str] | None = None,
        deletable: bool = False,
    ):
        super().__init__(parent)
        self.setWindowTitle(title)
        self.setModal(True)
        self.resize(560, 640)

        self._available_groups = sorted(set(available_groups or []), key=str.lower)
        self._selected_groups = list(initial_groups or [])

        self._img_qimage: QtGui.QImage | None = None

        v = QtWidgets.QVBoxLayout(self)
        v.setContentsMargins(12, 12, 12, 12)
        v.setSpacing(10)

        # Превью изображения + кнопка "Вставить из буфера"
        self.preview = QtWidgets.QLabel(alignment=QtCore.Qt.AlignmentFlag.AlignCenter)
        self.preview.setMinimumSize(self.PREVIEW_SIZE)
        self.preview.setFrameShape(QtWidgets.QFrame.Shape.StyledPanel)
        v.addWidget(self.preview)

        paste_btn = QtWidgets.QPushButton("Заменить из буфера обмена")
        paste_btn.clicked.connect(self._paste_from_clipboard)
        v.addWidget(paste_btn)

        if initial_image_path and os.path.isfile(initial_image_path):
            pix = QtGui.QPixmap(initial_image_path)
            if not pix.isNull():
                self._set_image(pix.toImage())

        # Поля: имя + описание
        form = QtWidgets.QFormLayout()
        form.setLabelAlignment(QtCore.Qt.AlignmentFlag.AlignLeft)
        form.setFormAlignment(QtCore.Qt.AlignmentFlag.AlignLeft)

        self.name_edit = QtWidgets.QLineEdit(initial_name or "")
        form.addRow("Имя:", self.name_edit)

        self.desc_edit = QtWidgets.QTextEdit()
        self.desc_edit.setAcceptRichText(False)
        self.desc_edit.setPlainText(initial_desc or "")
        self.desc_edit.setMinimumHeight(140)
        form.addRow("Описание:", self.desc_edit)

        v.addLayout(form)

        # Группы
        grp_box = QtWidgets.QGroupBox("Группы")
        gvl = QtWidgets.QVBoxLayout(grp_box)

        top_row = QtWidgets.QHBoxLayout()
        self.group_combo = QtWidgets.QComboBox()
        self.group_combo.setEditable(True)
        self.group_combo.addItems(self._available_groups)
        top_row.addWidget(QtWidgets.QLabel("Выбрать или ввести:"))
        top_row.addWidget(self.group_combo, 1)
        add_grp_btn = QtWidgets.QPushButton("Добавить")
        add_grp_btn.clicked.connect(self._on_group_add)
        top_row.addWidget(add_grp_btn)
        gvl.addLayout(top_row)

        list_row = QtWidgets.QHBoxLayout()
        self.group_list = QtWidgets.QListWidget()
        self.group_list.setSelectionMode(QtWidgets.QAbstractItemView.SelectionMode.ExtendedSelection)
        for g in self._selected_groups:
            self.group_list.addItem(g)
        list_row.addWidget(self.group_list, 1)

        btn_col = QtWidgets.QVBoxLayout()
        del_sel = QtWidgets.QPushButton("Удалить выбранные")
        del_sel.clicked.connect(self._on_group_remove_selected)
        btn_col.addWidget(del_sel)
        clr = QtWidgets.QPushButton("Очистить")
        clr.clicked.connect(self._on_group_clear)
        btn_col.addWidget(clr)
        btn_col.addStretch(1)
        list_row.addLayout(btn_col)
        gvl.addLayout(list_row)

        v.addWidget(grp_box)

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

    # --- Публичные геттеры для результата ---
    def name(self) -> str:
        return (self.name_edit.text() or "").strip()

    def description(self) -> str:
        return (self.desc_edit.toPlainText() or "").strip()

    def groups(self) -> List[str]:
        return [self.group_list.item(i).text() for i in range(self.group_list.count())]

    def image(self) -> QtGui.QImage | None:
        return self._img_qimage

    # --- Группы ---
    def _on_group_add(self):
        g = (self.group_combo.currentText() or "").strip()
        if not g:
            return
        existing = {self.group_list.item(i).text() for i in range(self.group_list.count())}
        if g not in existing:
            self.group_list.addItem(g)
        self.group_combo.setCurrentText("")

    def _on_group_remove_selected(self):
        for item in self.group_list.selectedItems():
            row = self.group_list.row(item)
            self.group_list.takeItem(row)

    def _on_group_clear(self):
        self.group_list.clear()

    # --- Картинка ---
    def _set_image(self, img: QtGui.QImage | None):
        self._img_qimage = img
        if img is None or img.isNull():
            self.preview.setPixmap(QtGui.QPixmap())  # очистить
            self.preview.setText("(нет изображения)")
            return
        pix = QtGui.QPixmap.fromImage(img)
        scaled = pix.scaled(self.PREVIEW_SIZE, QtCore.Qt.AspectRatioMode.KeepAspectRatio,
                            QtCore.Qt.TransformationMode.SmoothTransformation)
        self.preview.setText("")
        self.preview.setPixmap(scaled)

    def _paste_from_clipboard(self):
        cb = QtGui.QGuiApplication.clipboard()
        img = cb.image()
        if img.isNull():
            QtWidgets.QMessageBox.information(self, "Буфер обмена", "В буфере нет изображения.")
            return
        self._set_image(img)

    # --- Удаление ---
    def _on_delete_clicked(self):
        self.done(EditCharacterDialog.DeleteCode)
