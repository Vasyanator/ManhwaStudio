from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/widgets.py
Параметрические Qt-виджеты для узлов и селектор переменных.

Main items:
- `NumberStartParamsWidget`: параметры стартового числового цикла.
- `StringStartParamsWidget`: выбор txt-файла для строкового цикла.
- `VariableSelectorWidget`: combobox выбора переменной + статус persist.
"""

from typing import Optional

from PyQt6 import QtCore, QtWidgets

from .constants import DATA_TYPE_LABELS
from .models import VariableDefinition


class NumberStartParamsWidget(QtWidgets.QWidget):
    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QFormLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self.start_spin = QtWidgets.QSpinBox(self)
        self.start_spin.setRange(-999_999, 999_999)
        self.start_spin.setValue(0)

        self.step_spin = QtWidgets.QSpinBox(self)
        self.step_spin.setRange(-999_999, 999_999)
        self.step_spin.setValue(1)
        self.step_spin.setSpecialValueText("0 (нежелательно)")

        self.end_spin = QtWidgets.QSpinBox(self)
        self.end_spin.setRange(-999_999, 999_999)
        self.end_spin.setValue(10)

        layout.addRow("Начало:", self.start_spin)
        layout.addRow("Шаг:", self.step_spin)
        layout.addRow("Конец:", self.end_spin)


class StringStartParamsWidget(QtWidgets.QWidget):
    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        row = QtWidgets.QHBoxLayout()
        row.setContentsMargins(0, 0, 0, 0)
        row.setSpacing(4)

        self.path_edit = QtWidgets.QLineEdit(self)
        self.path_edit.setPlaceholderText("Выберите .txt файл со строками")
        self.path_edit.setToolTip("При каждом цикле узел будет выдавать следующую строку файла")
        row.addWidget(self.path_edit, 1)

        pick_button = QtWidgets.QPushButton("…", self)
        pick_button.setFixedWidth(30)
        pick_button.setToolTip("Выбрать txt файл")
        pick_button.clicked.connect(self._pick_txt_file)
        row.addWidget(pick_button)

        layout.addLayout(row)

    def _pick_txt_file(self) -> None:
        path, _ = QtWidgets.QFileDialog.getOpenFileName(
            self,
            "Выберите txt файл",
            "",
            "Text files (*.txt);;All files (*)",
        )
        if path:
            self.path_edit.setText(path)


class VariableSelectorWidget(QtWidgets.QWidget):
    variable_changed = QtCore.pyqtSignal(str)

    def __init__(self, mode_text: str, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(3)

        self._mode_label = QtWidgets.QLabel(mode_text, self)
        self._mode_label.setStyleSheet("color: #cbd5e1;")
        layout.addWidget(self._mode_label)

        self.combo = QtWidgets.QComboBox(self)
        self.combo.currentIndexChanged.connect(self._emit_current_name)
        layout.addWidget(self.combo)

        self.persist_label = QtWidgets.QLabel("", self)
        self.persist_label.setStyleSheet("color: #94a3b8;")
        layout.addWidget(self.persist_label)

        self.set_variables([])

    def set_variables(self, variables: list[VariableDefinition], selected_name: Optional[str] = None) -> None:
        previous = selected_name or self.current_variable_name()

        self.combo.blockSignals(True)
        self.combo.clear()
        if not variables:
            self.combo.addItem("<нет переменных>")
            self.combo.setEnabled(False)
        else:
            self.combo.setEnabled(True)
            for var in variables:
                label = f"{var.name} ({DATA_TYPE_LABELS.get(var.data_type, var.data_type)})"
                self.combo.addItem(label, var.name)

            target_name = previous if previous and any(v.name == previous for v in variables) else variables[0].name
            idx = self.combo.findData(target_name)
            if idx >= 0:
                self.combo.setCurrentIndex(idx)
        self.combo.blockSignals(False)
        self._emit_current_name()

    def current_variable_name(self) -> Optional[str]:
        value = self.combo.currentData()
        if isinstance(value, str):
            return value
        return None

    def _emit_current_name(self, *_args) -> None:
        self.variable_changed.emit(self.current_variable_name() or "")

    def set_persist_flag(self, value: Optional[bool]) -> None:
        if value is None:
            self.persist_label.setText("Сохранение между циклами: n/a")
            return
        self.persist_label.setText(
            "Сохранение между циклами: Да" if value else "Сохранение между циклами: Нет"
        )
