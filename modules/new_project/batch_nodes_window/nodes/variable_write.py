from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/variable_write.py
Фабрика узла `variable_write` и его шаблон для палитры.
"""

from typing import Callable, Optional

from ..graphics_items import VariableNodeBlockItem
from ..models import NodeTemplate, VariableDefinition


TEMPLATE = NodeTemplate(
    "variable_write",
    "Переменная (запись)",
    "Переменные",
    "Принимает data-значение и сохраняет в переменную",
)


def create_node(
    variable_resolver: Callable[[str], Optional[VariableDefinition]],
    variables: list[VariableDefinition],
    preferred_variable: Optional[str] = None,
) -> VariableNodeBlockItem:
    return VariableNodeBlockItem(
        mode="write",
        variable_resolver=variable_resolver,
        initial_variables=variables,
        selected_variable=preferred_variable,
    )
