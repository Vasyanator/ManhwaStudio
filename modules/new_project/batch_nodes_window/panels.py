from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/panels.py
Dock-панели для node-editor: палитра узлов и менеджер переменных.

Main items:
- `NodesPalettePanel`: вкладки категорий узлов + сигнал добавления.
- `VariablesPanel`: таблица и CRUD-панель переменных + кнопки read/write узлов.
"""

from typing import Optional

from PyQt6 import QtCore, QtWidgets

from .constants import DATA_TYPE_LABELS, TYPE_IMAGE_LIST, TYPE_INT, TYPE_STR
from .models import NodeTemplate, VariableDefinition


class NodesPalettePanel(QtWidgets.QWidget):
    add_node_requested = QtCore.pyqtSignal(str)

    def __init__(self, templates: list[NodeTemplate], parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        self._lists: list[QtWidgets.QListWidget] = []
        self._templates_by_key = {tpl.key: tpl for tpl in templates}

        layout = QtWidgets.QVBoxLayout(self)
        layout.setContentsMargins(6, 6, 6, 6)
        layout.setSpacing(6)

        self.tabs = QtWidgets.QTabWidget(self)
        self.tabs.setTabPosition(QtWidgets.QTabWidget.TabPosition.West)
        layout.addWidget(self.tabs, 1)

        by_category: dict[str, list[NodeTemplate]] = {}
        for tpl in templates:
            by_category.setdefault(tpl.category, []).append(tpl)

        for category in sorted(by_category.keys()):
            page = QtWidgets.QWidget(self.tabs)
            page_layout = QtWidgets.QVBoxLayout(page)
            page_layout.setContentsMargins(4, 4, 4, 4)
            page_layout.setSpacing(4)

            node_list = QtWidgets.QListWidget(page)
            node_list.setAlternatingRowColors(True)
            for tpl in sorted(by_category[category], key=lambda x: x.title):
                item = QtWidgets.QListWidgetItem(tpl.title)
                item.setData(QtCore.Qt.ItemDataRole.UserRole, tpl.key)
                item.setToolTip(tpl.description)
                node_list.addItem(item)
            node_list.itemDoubleClicked.connect(self._emit_from_item)
            page_layout.addWidget(node_list, 1)

            add_button = QtWidgets.QPushButton("Добавить выбранный узел", page)
            add_button.clicked.connect(lambda _, lst=node_list: self._emit_from_list(lst))
            page_layout.addWidget(add_button)

            self._lists.append(node_list)
            self.tabs.addTab(page, category)

    def _emit_from_item(self, item: QtWidgets.QListWidgetItem) -> None:
        key = item.data(QtCore.Qt.ItemDataRole.UserRole)
        if isinstance(key, str):
            self.add_node_requested.emit(key)

    def _emit_from_list(self, node_list: QtWidgets.QListWidget) -> None:
        item = node_list.currentItem()
        if item is not None:
            self._emit_from_item(item)


class VariablesPanel(QtWidgets.QWidget):
    variable_add_requested = QtCore.pyqtSignal(str, str, bool)
    variable_remove_requested = QtCore.pyqtSignal(str)
    add_read_node_requested = QtCore.pyqtSignal(str)
    add_write_node_requested = QtCore.pyqtSignal(str)

    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QVBoxLayout(self)
        layout.setContentsMargins(6, 6, 6, 6)
        layout.setSpacing(6)

        self.table = QtWidgets.QTableWidget(0, 3, self)
        self.table.setHorizontalHeaderLabels(["Имя", "Тип", "Сохр. между циклами"])
        self.table.horizontalHeader().setStretchLastSection(True)
        self.table.horizontalHeader().setSectionResizeMode(0, QtWidgets.QHeaderView.ResizeMode.Stretch)
        self.table.horizontalHeader().setSectionResizeMode(1, QtWidgets.QHeaderView.ResizeMode.ResizeToContents)
        self.table.horizontalHeader().setSectionResizeMode(2, QtWidgets.QHeaderView.ResizeMode.ResizeToContents)
        self.table.setSelectionBehavior(QtWidgets.QAbstractItemView.SelectionBehavior.SelectRows)
        self.table.setSelectionMode(QtWidgets.QAbstractItemView.SelectionMode.SingleSelection)
        self.table.setEditTriggers(QtWidgets.QAbstractItemView.EditTrigger.NoEditTriggers)
        layout.addWidget(self.table, 1)

        form_group = QtWidgets.QGroupBox("Создание переменной", self)
        form_layout = QtWidgets.QFormLayout(form_group)
        form_layout.setContentsMargins(6, 6, 6, 6)

        self.name_edit = QtWidgets.QLineEdit(form_group)
        self.name_edit.setPlaceholderText("например: chapter_url")

        self.type_combo = QtWidgets.QComboBox(form_group)
        self.type_combo.addItem("int", TYPE_INT)
        self.type_combo.addItem("str", TYPE_STR)
        self.type_combo.addItem("список картинок", TYPE_IMAGE_LIST)

        self.persist_checkbox = QtWidgets.QCheckBox("Сохранять между циклами", form_group)

        form_layout.addRow("Имя:", self.name_edit)
        form_layout.addRow("Тип:", self.type_combo)
        form_layout.addRow("", self.persist_checkbox)
        layout.addWidget(form_group)

        controls = QtWidgets.QGridLayout()
        add_button = QtWidgets.QPushButton("Создать", self)
        delete_button = QtWidgets.QPushButton("Удалить выбранную", self)
        add_read_button = QtWidgets.QPushButton("Добавить узел чтения", self)
        add_write_button = QtWidgets.QPushButton("Добавить узел записи", self)

        add_button.clicked.connect(self._on_add_clicked)
        delete_button.clicked.connect(self._on_delete_clicked)
        add_read_button.clicked.connect(self._on_add_read_node_clicked)
        add_write_button.clicked.connect(self._on_add_write_node_clicked)

        controls.addWidget(add_button, 0, 0)
        controls.addWidget(delete_button, 0, 1)
        controls.addWidget(add_read_button, 1, 0)
        controls.addWidget(add_write_button, 1, 1)
        layout.addLayout(controls)

    def set_variables(self, variables: list[VariableDefinition]) -> None:
        self.table.setRowCount(len(variables))
        for row, var in enumerate(variables):
            self.table.setItem(row, 0, QtWidgets.QTableWidgetItem(var.name))
            self.table.setItem(row, 1, QtWidgets.QTableWidgetItem(DATA_TYPE_LABELS.get(var.data_type, var.data_type)))
            self.table.setItem(row, 2, QtWidgets.QTableWidgetItem("Да" if var.persist_between_cycles else "Нет"))

    def selected_variable_name(self) -> Optional[str]:
        row = self.table.currentRow()
        if row < 0:
            return None
        item = self.table.item(row, 0)
        if item is None:
            return None
        return item.text().strip() or None

    def _on_add_clicked(self) -> None:
        name = (self.name_edit.text() or "").strip()
        if not name:
            QtWidgets.QMessageBox.warning(self, "Переменные", "Введите имя переменной.")
            return
        data_type = self.type_combo.currentData()
        if not isinstance(data_type, str):
            return
        self.variable_add_requested.emit(name, data_type, self.persist_checkbox.isChecked())
        self.name_edit.clear()

    def _on_delete_clicked(self) -> None:
        name = self.selected_variable_name()
        if not name:
            QtWidgets.QMessageBox.information(self, "Переменные", "Выберите переменную для удаления.")
            return
        self.variable_remove_requested.emit(name)

    def _on_add_read_node_clicked(self) -> None:
        name = self.selected_variable_name()
        if not name:
            QtWidgets.QMessageBox.information(self, "Переменные", "Выберите переменную.")
            return
        self.add_read_node_requested.emit(name)

    def _on_add_write_node_clicked(self) -> None:
        name = self.selected_variable_name()
        if not name:
            QtWidgets.QMessageBox.information(self, "Переменные", "Выберите переменную.")
            return
        self.add_write_node_requested.emit(name)
