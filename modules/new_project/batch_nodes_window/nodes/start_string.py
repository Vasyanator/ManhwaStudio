from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/start_string.py
Фабрика узла `start_string` и его шаблон для палитры.
"""

from ..constants import KIND_DATA, KIND_EXEC, TYPE_STR
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec
from ..widgets import StringStartParamsWidget


TEMPLATE = NodeTemplate(
    "start_string",
    "Старт (строка)",
    "Старт",
    "Цикл по строкам txt-файла: выдаёт exec + str",
)


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Старт (строка)",
        [
            SocketSpec("Далее", "out", KIND_EXEC),
            SocketSpec("Строка", "out", KIND_DATA, data_type=TYPE_STR),
        ],
        params_widget=StringStartParamsWidget(),
        description="for-цикл по строкам txt-файла",
        width=340.0,
    )
