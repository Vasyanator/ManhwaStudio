from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/start_number.py
Фабрика узла `start_number` и его шаблон для палитры.
"""

from ..constants import KIND_DATA, KIND_EXEC, TYPE_INT
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec
from ..widgets import NumberStartParamsWidget


TEMPLATE = NodeTemplate(
    "start_number",
    "Старт (число)",
    "Старт",
    "Цикл for по числам: начало/шаг/конец, выдаёт exec + int",
)


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Старт (число)",
        [
            SocketSpec("Далее", "out", KIND_EXEC),
            SocketSpec("Индекс", "out", KIND_DATA, data_type=TYPE_INT),
        ],
        params_widget=NumberStartParamsWidget(),
        description="for-цикл по числам: начало/шаг/конец",
        width=320.0,
    )
