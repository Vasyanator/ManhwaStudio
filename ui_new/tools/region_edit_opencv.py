# ui_new/tools/region_edit_cv.py
from __future__ import annotations
from typing import Optional

import numpy as np

from PyQt6.QtCore import Qt
from PyQt6.QtWidgets import QLabel, QComboBox, QFormLayout, QGroupBox, QSpinBox, QWidget
from PyQt6.QtGui import QImage

from .base import RegionEditorDialog, RegionEditTool

# ---------------------- Диалог редактирования с OpenCV Inpaint ----------------------
class CVInpaintEditorDialog(RegionEditorDialog):
    """
    Редактор области с маской и встроенным OpenCV inpaint.
    Методы: Telea (cv2.INPAINT_TELEA) и Navier–Stokes (cv2.INPAINT_NS).
    """
    def __init__(self, image: QImage, parent: Optional[QWidget] = None):
        super().__init__(image, parent)
        self.setWindowTitle("Редактор области (OpenCV inpaint)")
        try:
            import cv2  # noqa: F401
            self.set_status("Готово. Нарисуйте маску и нажмите «Обработать».")
        except Exception as e:
            self.btn_process.setEnabled(False)
            self.set_status(f"❌ OpenCV недоступен: {e}")

    def info_text(self) -> str:
        return "Модель: OpenCV Inpaint (CPU)"

    def build_params_block(self):
        params_group = QGroupBox("Параметры OpenCV inpaint")
        params_form = QFormLayout()

        self.combo_method = QComboBox()
        self.combo_method.addItem("Telea")
        self.combo_method.addItem("Navier–Stokes")

        self.radius_spin = QSpinBox()
        self.radius_spin.setRange(1, 100)
        self.radius_spin.setValue(3)

        params_form.addRow("Метод:", self.combo_method)
        params_form.addRow("Радиус:", self.radius_spin)
        params_group.setLayout(params_form)
        return params_group

    def run(self, base_rgb: np.ndarray, mask_a: np.ndarray):
        import cv2

        img_bgr = base_rgb[..., ::-1].copy()
        mask_bin = (mask_a > 0).astype(np.uint8) * 255

        method_name = self.combo_method.currentText()
        method_flag = cv2.INPAINT_TELEA if method_name == "Telea" else cv2.INPAINT_NS
        radius = float(self.radius_spin.value())

        result_bgr = cv2.inpaint(img_bgr, mask_bin, radius, method_flag)
        result_rgb = result_bgr[..., ::-1]
        self.set_status(f"✅ Готово ({method_name}, r={int(radius)})")
        return result_rgb


# ---------------------- Инструмент ----------------------
class RegionEditCVtool(RegionEditTool):
    """
    Инструмент редактирования области с OpenCV inpaint.

    Использование:
      • Shift+ЛКМ — прямоугольник на картинке (как в скелете).
      • Откроется диалог, рисуем маску и жмём «Обработать».
      • Выбираем метод (Telea/Navier–Stokes) и радиус.
      • «Применить» — вставит результат точно в выбранную область.
    """
    tool_id = "region_edit_cv"
    title   = "Заполнение OpenCV"

    def create_editor_dialog(self, image: QImage, parent=None) -> RegionEditorDialog:
        return CVInpaintEditorDialog(image, parent)

    def build_ui(self, parent_layout) -> None:
        hint = QLabel("Выделение: Shift + ЛКМ (прямоугольник)")
        hint.setStyleSheet("color: #666;")
        parent_layout.addWidget(hint)

        info = QLabel("Методы: Telea (быстрее, сглаженно) • Navier–Stokes (структуры/градиенты)")
        info.setStyleSheet("color:#888; font-size:10px;")
        parent_layout.addWidget(info)
