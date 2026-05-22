from __future__ import annotations
from PyQt6.QtWidgets import QHBoxLayout, QLabel, QSlider
from PyQt6.QtCore import Qt
from .base import BaseTool

class EraserTool(BaseTool):
    tool_id = "eraser"
    title = "Ластик"

    def activate(self, view) -> None:
        super().activate(view)
        if hasattr(view, "set_tool"):
            view.set_tool("eraser")

    def build_ui(self, parent) -> None:
        lay = parent if isinstance(parent, QHBoxLayout) else parent.layout()
        size_lbl = QLabel("Размер:")
        size_sld = QSlider(Qt.Orientation.Horizontal)
        size_sld.setMinimum(1); size_sld.setMaximum(200)
        size_sld.setValue(self.view.brush_radius)
        size_sld.setFixedWidth(140)
        size_sld.valueChanged.connect(lambda v: self.view.set_brush_radius(v))
        lay.addWidget(size_lbl); lay.addWidget(size_sld)
