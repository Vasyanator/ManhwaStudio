from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/constants.py
Общие константы типов сокетов/данных и цвета для визуализации линий/портов.

Main items:
- `KIND_EXEC`, `KIND_DATA`: типы соединений (выполнение/данные).
- `TYPE_INT`, `TYPE_STR`, `TYPE_IMAGE_LIST`: поддерживаемые data-типы.
- `DATA_TYPE_LABELS`: человеко-читаемые подписи типов.
- `data_type_color`/`socket_color`: единый выбор цвета сокета/линии.
"""

from typing import Optional

from PyQt6 import QtGui


KIND_EXEC = "exec"
KIND_DATA = "data"

TYPE_INT = "int"
TYPE_STR = "str"
TYPE_IMAGE_LIST = "image_list"

DATA_TYPE_LABELS = {
    TYPE_INT: "int",
    TYPE_STR: "str",
    TYPE_IMAGE_LIST: "список картинок",
}


def data_type_color(data_type: Optional[str]) -> QtGui.QColor:
    if data_type == TYPE_INT:
        return QtGui.QColor("#60a5fa")
    if data_type == TYPE_STR:
        return QtGui.QColor("#fb923c")
    if data_type == TYPE_IMAGE_LIST:
        return QtGui.QColor("#34d399")
    return QtGui.QColor("#c084fc")


def socket_color(kind: str, data_type: Optional[str]) -> QtGui.QColor:
    if kind == KIND_EXEC:
        return QtGui.QColor("#facc15")
    return data_type_color(data_type)
