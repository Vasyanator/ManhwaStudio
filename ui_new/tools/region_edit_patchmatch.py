# ui_new/tools/region_edit_patchmatch.py
from __future__ import annotations
from typing import Optional

import numpy as np

from PyQt6.QtWidgets import QLabel, QFormLayout, QWidget, QGroupBox, QSpinBox
from PyQt6.QtGui import QImage

from .base import RegionEditorDialog, RegionEditTool


# ---------------------- Диалог редактирования с PatchMatch Inpaint ----------------------
class PatchMatchInpaintEditorDialog(RegionEditorDialog):
    """
    Редактор области с маской и PatchMatch-based inpaint.

    Бэкенд: PyPI пакет "PyPatchMatch" (InvokeAI Project).
    Python API: `import patch_match; patch_match.inpaint(image, mask, patch_size=3)`
    """

    def __init__(self, image: QImage, parent: Optional[QWidget] = None):
        super().__init__(image, parent)
        self.setWindowTitle("Редактор области (PatchMatch inpaint)")
        try:
            import patchmatch  # noqa: F401
            if not getattr(patchmatch, "patchmatch_available", True):
                self.btn_process.setEnabled(False)
                self.set_status("❌ PatchMatch недоступен (patchmatch_available=False).")
        except Exception as e:
            self.btn_process.setEnabled(False)
            self.set_status(f"❌ PyPatchMatch/patch_match недоступен: {e}")

    def info_text(self) -> str:
        return "Модель: PatchMatch Inpaint (CPU)"

    def build_params_block(self):
        params_group = QGroupBox("Параметры PatchMatch inpaint")
        params_form = QFormLayout()

        self.patch_size_spin = QSpinBox()
        self.patch_size_spin.setRange(1, 99)
        self.patch_size_spin.setSingleStep(2)
        self.patch_size_spin.setValue(3)

        hint = QLabel("Обычно нечётное. Больше → сильнее «копирование текстур», но медленнее.")
        hint.setStyleSheet("color:#888; font-size:10px;")

        params_form.addRow("Patch size:", self.patch_size_spin)
        params_form.addRow("", hint)
        params_group.setLayout(params_form)
        return params_group

    def run(self, base_rgb: np.ndarray, mask_a: np.ndarray):
        import patchmatch

        if not getattr(patchmatch, "patchmatch_available", True):
            raise RuntimeError("PatchMatch недоступен (patchmatch_available=False)")

        mask_bin = (mask_a > 0).astype(np.uint8) * 255

        patch_size = int(max(1, self.patch_size_spin.value()))

        result = patchmatch.inpaint_regularity(base_rgb, mask_bin, patch_size=patch_size)
        result_rgb = np.asarray(result)
        if result_rgb.dtype != np.uint8:
            result_rgb = np.clip(result_rgb, 0, 255).astype(np.uint8)
        if result_rgb.ndim != 3 or result_rgb.shape[2] != 3:
            raise ValueError(f"Unexpected result shape from patch_match.inpaint: {result_rgb.shape}")

        self.set_status(f"✅ Готово (PatchMatch, patch_size={patch_size})")
        return result_rgb


# ---------------------- Инструмент ----------------------
class RegionEditPatchMatchTool(RegionEditTool):
    """
    Инструмент редактирования области с PatchMatch inpaint.

    Использование:
      • Shift+ЛКМ — прямоугольник на картинке (как в скелете).
      • Откроется диалог, рисуем маску и жмём «Обработать».
      • Patch size — размер патча (обычно нечётный).
      • «Применить» — вставит результат точно в выбранную область.
    """
    tool_id = "region_edit_patchmatch"
    title   = "Заполнение PatchMatch"

    def create_editor_dialog(self, image: QImage, parent=None) -> RegionEditorDialog:
        return PatchMatchInpaintEditorDialog(image, parent)

    def build_ui(self, parent_layout) -> None:
        hint = QLabel("Выделение: Shift + ЛКМ (прямоугольник)")
        hint.setStyleSheet("color: #666;")
        parent_layout.addWidget(hint)

        info = QLabel("Метод: PatchMatch (patch-based, без ИИ). Лучше для текстур и больших дыр, чем Telea/NS.")
        info.setStyleSheet("color:#888; font-size:10px;")
        parent_layout.addWidget(info)
