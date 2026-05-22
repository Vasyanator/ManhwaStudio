from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/stitch_split.py
Узел `stitch_split` для массовой обработки.

Main items:
- `StitchSplitParamsWidget`: параметры как в панели "Сшивание/Нарезка" (без "Нарезать как главу").
- `create_node`: фабрика узла `Склейка/резка` (exec + image_list -> exec + image_list).
"""

from typing import Optional

from PyQt6 import QtWidgets

from ..constants import KIND_DATA, KIND_EXEC, TYPE_IMAGE_LIST
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "stitch_split",
    "Склейка/резка",
    "Обработка",
    "Склеивает и нарезает список картинок тем же алгоритмом, что и панель Сшивания/Нарезки.",
)


class StitchSplitParamsWidget(QtWidgets.QWidget):
    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QFormLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self.k_edit = QtWidgets.QLineEdit(self)
        self.k_edit.setPlaceholderText("пусто = авто")

        self.hmax_edit = QtWidgets.QLineEdit("19000", self)
        self.band_edit = QtWidgets.QLineEdit("4", self)
        self.tol_edit = QtWidgets.QLineEdit("15", self)
        self.radius_edit = QtWidgets.QLineEdit("5500", self)
        self.prefer_up_checkbox = QtWidgets.QCheckBox("Сначала вверх при refine", self)
        self.prefer_up_checkbox.setChecked(True)

        layout.addRow("K:", self.k_edit)
        layout.addRow("Hmax:", self.hmax_edit)
        layout.addRow("band_rows:", self.band_edit)
        layout.addRow("tol:", self.tol_edit)
        layout.addRow("search_radius:", self.radius_edit)
        layout.addRow("", self.prefer_up_checkbox)


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Склейка/резка",
        [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("Картинки", "in", KIND_DATA, data_type=TYPE_IMAGE_LIST),
            SocketSpec("Далее", "out", KIND_EXEC),
            SocketSpec("Картинки", "out", KIND_DATA, data_type=TYPE_IMAGE_LIST),
        ],
        params_widget=StitchSplitParamsWidget(),
        description="Склейка и повторная нарезка списка изображений",
        width=360.0,
    )
