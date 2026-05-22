from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/quick_downloader.py
Фабрика узла `quick_downloader` и его шаблон для палитры.
"""

from ..constants import KIND_DATA, KIND_EXEC, TYPE_IMAGE_LIST, TYPE_STR
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "quick_downloader",
    "Быстрый выкачиватель",
    "I/O",
    "Принимает ссылку (str), выдаёт список картинок",
)


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Быстрый выкачиватель",
        [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("Ссылка", "in", KIND_DATA, data_type=TYPE_STR),
            SocketSpec("Далее", "out", KIND_EXEC),
            SocketSpec("Картинки", "out", KIND_DATA, data_type=TYPE_IMAGE_LIST),
        ],
        description="Скачивает картинки по ссылке",
        width=330.0,
    )
