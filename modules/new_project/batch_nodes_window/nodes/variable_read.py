from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/variable_read.py
Фабрика узла `variable_read` и его шаблон для палитры.
"""

from typing import Callable, Optional

from ..graphics_items import VariableNodeBlockItem
from ..models import NodeTemplate, VariableDefinition


TEMPLATE = NodeTemplate(
    "variable_read",
    "Переменная (чтение)",
    "Переменные",
    "Читает значение переменной и выдаёт его как data-выход",
)


def create_node(
    variable_resolver: Callable[[str], Optional[VariableDefinition]],
    variables: list[VariableDefinition],
    preferred_variable: Optional[str] = None,
) -> VariableNodeBlockItem:
    return VariableNodeBlockItem(
        mode="read",
        variable_resolver=variable_resolver,
        initial_variables=variables,
        selected_variable=preferred_variable,
    )
