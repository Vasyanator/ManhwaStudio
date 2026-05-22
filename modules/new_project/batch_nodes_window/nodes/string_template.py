from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/string_template.py
Узел шаблонизатора строки:
- принимает строку-шаблон с `{переменными}`;
- динамически создаёт data-входы по именам переменных;
- имеет exec-вход/exec-выход для встраивания в поток выполнения;
- каждый вход принимает `int` или `str`;
- выдаёт итоговую строку (`str`).
"""

import re
from collections import OrderedDict
from typing import Optional

from PyQt6 import QtCore, QtWidgets

from ..constants import KIND_DATA, KIND_EXEC, TYPE_INT, TYPE_STR
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "string_template",
    "Шаблонизатор строки",
    "Строки",
    "Собирает строку по шаблону с переменными в фигурных скобках: {name}",
)

_PLACEHOLDER_RE = re.compile(r"\{([^{}]+)\}")


class StringTemplateParamsWidget(QtWidgets.QWidget):
    template_changed = QtCore.pyqtSignal(str)

    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self.template_edit = QtWidgets.QLineEdit(self)
        self.template_edit.setPlaceholderText("Например: Привет, {name}! Глава {index}")
        self.template_edit.setToolTip("Имена внутри {...} создают динамические входы узла")
        self.template_edit.textChanged.connect(self.template_changed.emit)
        layout.addWidget(self.template_edit)

    def template_text(self) -> str:
        return self.template_edit.text() or ""

    def set_template_text(self, text: str) -> None:
        self.template_edit.setText(text)


class StringTemplateNodeBlockItem(NodeBlockItem):
    def __init__(self, initial_template: str = "Привет, {name}"):
        self._params_widget = StringTemplateParamsWidget()
        self._placeholder_names: list[str] = []
        super().__init__(
            "Шаблонизатор строки",
            [],
            params_widget=self._params_widget,
            description="Подставляет входные значения int/str в шаблон и выдаёт строку",
            width=360.0,
        )
        self._params_widget.template_changed.connect(self._on_template_changed)
        self._params_widget.set_template_text(initial_template)

    def _on_template_changed(self, template_text: str) -> None:
        placeholder_names = self._extract_placeholder_names(template_text)
        if placeholder_names == self._placeholder_names:
            return
        self._placeholder_names = placeholder_names
        self.rebuild_sockets(self._build_sockets())

    def _build_sockets(self) -> list[SocketSpec]:
        exec_sockets = [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("Далее", "out", KIND_EXEC),
        ]
        variable_inputs = [
            SocketSpec(
                name=name,
                direction="in",
                kind=KIND_DATA,
                accepted_data_types=(TYPE_INT, TYPE_STR),
            )
            for name in self._placeholder_names
        ]
        return [exec_sockets[0], *variable_inputs, exec_sockets[1], SocketSpec("Строка", "out", KIND_DATA, data_type=TYPE_STR)]

    @staticmethod
    def _extract_placeholder_names(template_text: str) -> list[str]:
        ordered_names: OrderedDict[str, None] = OrderedDict()
        for raw_name in _PLACEHOLDER_RE.findall(template_text):
            name = raw_name.strip()
            if not name:
                continue
            ordered_names[name] = None
        return list(ordered_names.keys())


def create_node() -> NodeBlockItem:
    return StringTemplateNodeBlockItem()
