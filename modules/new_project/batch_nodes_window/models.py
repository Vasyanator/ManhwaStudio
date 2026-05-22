from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/models.py
Минимальные модели node-editor, используемые UI и фабрикой узлов.

Main items:
- `VariableDefinition`: описание переменной (имя/тип/persist).
- `SocketSpec`: описание сокета узла (сторона, kind, data_type/allowed types).
- `NodeTemplate`: карточка узла для палитры (key/title/category/description).
"""

from dataclasses import dataclass
from typing import Optional


@dataclass
class VariableDefinition:
    name: str
    data_type: str
    persist_between_cycles: bool


@dataclass
class SocketSpec:
    name: str
    direction: str  # "in" | "out"
    kind: str  # exec | data
    data_type: Optional[str] = None
    accepted_data_types: tuple[str, ...] = ()
    allow_multiple: bool = False


@dataclass(frozen=True)
class NodeTemplate:
    key: str
    title: str
    category: str
    description: str
