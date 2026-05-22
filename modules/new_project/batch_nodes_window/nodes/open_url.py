from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/open_url.py
Узел `open_url` для массовой обработки.

Main items:
- `OpenUrlParamsWidget`: выбор браузера (как в продвинутом выкачивателе).
- `create_node`: фабрика узла `Открыть URL` (exec + str -> exec).
"""

from typing import Optional

from PyQt6 import QtWidgets

from ...downloaders import detect_available_browsers
from ..constants import KIND_DATA, KIND_EXEC, TYPE_STR
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "open_url",
    "Открыть URL",
    "Браузер",
    "Открывает URL в выбранном браузере и продолжает после полной прогрузки страницы.",
)


class OpenUrlParamsWidget(QtWidgets.QWidget):
    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QFormLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self.browser_combo = QtWidgets.QComboBox(self)
        browsers = detect_available_browsers()
        if not browsers:
            browsers = ["Firefox", "Chrome", "Edge", "Safari"]
        self.browser_combo.addItems(browsers)
        layout.addRow("Браузер:", self.browser_combo)

    def selected_browser(self) -> str:
        return (self.browser_combo.currentText() or "").strip()


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Открыть URL",
        [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("URL", "in", KIND_DATA, data_type=TYPE_STR),
            SocketSpec("Далее", "out", KIND_EXEC),
        ],
        params_widget=OpenUrlParamsWidget(),
        description="Переход по URL в выбранном браузере",
        width=330.0,
    )
