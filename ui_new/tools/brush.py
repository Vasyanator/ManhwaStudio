from __future__ import annotations
from PyQt6.QtWidgets import QWidget, QHBoxLayout, QLabel, QSlider, QPushButton, QColorDialog
from PyQt6.QtCore import Qt
from PyQt6.QtGui import QColor
from .base import BaseTool

class BrushTool(BaseTool):
    tool_id = "brush"
    title = "Кисть"

    def activate(self, view) -> None:
        super().activate(view)
        # активируем режим кисти
        if hasattr(view, "set_tool"):
            view.set_tool("brush")

    def build_ui(self, parent) -> None:
        # parent — это QHBoxLayout
        lay = parent if isinstance(parent, QHBoxLayout) else parent.layout()

        size_lbl = QLabel("Размер:")
        size_sld = QSlider(Qt.Orientation.Horizontal)
        size_sld.setMinimum(1); size_sld.setMaximum(200)
        size_sld.setValue(self.view.brush_radius)
        size_sld.setFixedWidth(140)
        size_sld.valueChanged.connect(lambda v: self.view.set_brush_radius(v))

        op_lbl = QLabel("Непрозр.:")
        op_sld = QSlider(Qt.Orientation.Horizontal)
        op_sld.setMinimum(0); op_sld.setMaximum(255)
        op_sld.setValue(self.view.brush_color.alpha())
        op_sld.setFixedWidth(140)
        op_sld.valueChanged.connect(lambda v: self.view.set_brush_opacity(v))

        clr_btn = QPushButton("Цвет…")
        clr_btn.clicked.connect(self._pick_color)

        lay.addWidget(size_lbl); lay.addWidget(size_sld)
        lay.addWidget(op_lbl); lay.addWidget(op_sld)
        lay.addWidget(clr_btn)

    def _pick_color(self):
        col = QColorDialog.getColor(self.view.brush_color, None, "Цвет кисти")
        if col.isValid():
            self.view.set_brush_color(QColor(col))
