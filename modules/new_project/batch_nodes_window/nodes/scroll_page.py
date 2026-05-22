from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/scroll_page.py
Узел `scroll_page` для массовой обработки.

Main items:
- `create_node`: фабрика узла `Промотать страницу` (только exec-поток).
"""

from ..constants import KIND_EXEC
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "scroll_page",
    "Промотать страницу",
    "Браузер",
    "Плавно проматывает страницу вниз до конца, вверх к началу и ещё раз вниз до конца.",
)


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Промотать страницу",
        [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("Далее", "out", KIND_EXEC),
        ],
        description="Плавно скроллит текущую вкладку вниз до конца, вверх к началу и снова вниз до конца",
        width=300.0,
    )
