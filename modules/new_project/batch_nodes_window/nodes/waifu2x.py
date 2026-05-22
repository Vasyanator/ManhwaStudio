from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/waifu2x.py
Узел `waifu2x` для массовой обработки.

Main items:
- `Waifu2xParamsWidget`: параметры шумоподавления/масштаба/tile и опционального пути к exe.
- `create_node`: фабрика узла `waifu2x` (exec + image_list -> exec + image_list).
"""

from typing import Optional

from PyQt6 import QtWidgets

from ..constants import KIND_DATA, KIND_EXEC, TYPE_IMAGE_LIST
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "waifu2x",
    "waifu2x",
    "Обработка",
    "Прогоняет список картинок через waifu2x-ncnn-vulkan.",
)


class Waifu2xParamsWidget(QtWidgets.QWidget):
    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QFormLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self.noise_combo = QtWidgets.QComboBox(self)
        self.noise_combo.addItems(["-1", "0", "1", "2", "3"])
        self.noise_combo.setCurrentText("3")

        self.scale_combo = QtWidgets.QComboBox(self)
        self.scale_combo.addItems(["1", "2", "4", "8", "16", "32"])
        self.scale_combo.setCurrentText("1")

        self.tile_edit = QtWidgets.QLineEdit("384", self)
        self.exec_path_edit = QtWidgets.QLineEdit(self)
        self.exec_path_edit.setPlaceholderText("пусто = путь по умолчанию")

        layout.addRow("Шум -n:", self.noise_combo)
        layout.addRow("Масштаб -s:", self.scale_combo)
        layout.addRow("Tile -t:", self.tile_edit)
        layout.addRow("Путь к exe:", self.exec_path_edit)

    def noise(self) -> int:
        return int(self.noise_combo.currentText())

    def scale(self) -> int:
        return int(self.scale_combo.currentText())

    def tile(self) -> int:
        return int((self.tile_edit.text() or "").strip())

    def exec_path(self) -> str:
        return (self.exec_path_edit.text() or "").strip()


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "waifu2x",
        [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("Картинки", "in", KIND_DATA, data_type=TYPE_IMAGE_LIST),
            SocketSpec("Далее", "out", KIND_EXEC),
            SocketSpec("Картинки", "out", KIND_DATA, data_type=TYPE_IMAGE_LIST),
        ],
        params_widget=Waifu2xParamsWidget(),
        description="Шумоподавление/апскейл списка картинок через waifu2x",
        width=360.0,
    )
