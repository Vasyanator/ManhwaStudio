from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/save_folder.py
Фабрика узла `save_folder` и его шаблон для палитры.
"""

from ..constants import KIND_DATA, KIND_EXEC, TYPE_IMAGE_LIST, TYPE_STR
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "save_folder",
    "Сохранение в папку",
    "I/O",
    "Принимает список картинок и путь (str), сохраняет файлы",
)


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Сохранение в папку",
        [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("Картинки", "in", KIND_DATA, data_type=TYPE_IMAGE_LIST),
            SocketSpec("Путь", "in", KIND_DATA, data_type=TYPE_STR),
            SocketSpec("Далее", "out", KIND_EXEC),
        ],
        description="Сохраняет список картинок по указанному пути",
        width=330.0,
    )
