from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/end.py
Фабрика завершающего узла `end` и его шаблон для палитры.
"""

from ..constants import KIND_EXEC
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "end",
    "Конец",
    "Поток",
    "Завершающий узел. При множественных exec-входах ожидает все ветки.",
)


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Конец",
        [SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True)],
        description="Завершение после достижения всех входящих веток выполнения",
        width=300.0,
    )
