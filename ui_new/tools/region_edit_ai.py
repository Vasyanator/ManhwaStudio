# ui_new/tools/region_edit_ai.py
from __future__ import annotations
from typing import Optional

import glob
import os
import numpy as np
import traceback
import threading

from PyQt6.QtCore import Qt, pyqtSignal, QSize
from PyQt6.QtGui import QImage
from PyQt6.QtWidgets import (
    QLabel, QCheckBox, QSpinBox, QGroupBox, QFormLayout, QMessageBox, QPushButton
)
from .base import (
    RegionEditorDialog,
    RegionEditTool,
    MaskCanvas,
    qimage_to_numpy_rgb,
    qimage_alpha_mask,
    numpy_rgb_to_qimage,
)
from config import program_dir, LAMA_DIR

# ---------------------- служебное: lazy-загрузка инпейнтера ----------------------
_INPAINTER = None
_INPAINTER_DEVICE = None
_INPAINTER_LOCK = threading.Lock()

def _validate_lama_setup():
    """
    Проверяем, что репозиторий и модель LaMa установлены перед загрузкой инпейнтера.
    """
    repo_path = os.path.join(program_dir, "lama_modernised")
    if not os.path.isdir(repo_path):
        raise FileNotFoundError("Репозиторий Vasyanator/lama_modernised не установлен. Установите его в папку программы.")

    inpainter_path = os.path.join(repo_path, "inpainter_v2.py")
    if not os.path.isfile(inpainter_path):
        raise FileNotFoundError("файл inpainter_v2.py не обнаружен в репозитории lama_modernised. Скопируйте его из папки lama_files.")

    config_path = os.path.join(LAMA_DIR, "config.yaml")
    models_dir = os.path.join(LAMA_DIR, "models")
    ckpt_files = glob.glob(os.path.join(models_dir, "*.ckpt")) if os.path.isdir(models_dir) else []
    if not os.path.isfile(config_path) or not ckpt_files:
        raise FileNotFoundError("Модель Lama не скачана или повреждена. Удалите папку AI_models/Lama и скачайте модель в менеджере моделей ИИ.")

def _get_inpainter(device: str):
    """
    Ленивая загрузка модели инпейнта (InpainterV2).
    Устройство выбирается автоматически.
    """
    global _INPAINTER
    global _INPAINTER_DEVICE
    with _INPAINTER_LOCK:
        if _INPAINTER is None or _INPAINTER_DEVICE != device:
            if _INPAINTER is not None:
                try:
                    _INPAINTER.unload(clear_cache=True)
                except Exception:
                    pass
                _INPAINTER = None
            _validate_lama_setup()
            # Используем InpainterV2
            from lama_modernised.inpainter_v2 import InpainterV2
            _INPAINTER = InpainterV2(device=device, refine=False, verbose=False)
            _INPAINTER_DEVICE = device
        return _INPAINTER


def _unload_inpainter():
    """Выгрузить инпейнтер из памяти GPU"""
    global _INPAINTER
    global _INPAINTER_DEVICE
    with _INPAINTER_LOCK:
        if _INPAINTER is not None:
            try:
                _INPAINTER.unload(clear_cache=True)
                _INPAINTER = None
                _INPAINTER_DEVICE = None
                return True
            except Exception as e:
                print(f"Ошибка при выгрузке инпейнтера: {e}")
                return False
        return False


# ---------------------- Диалог редактирования с инпейнтом ----------------------
class InpaintEditorDialog(RegionEditorDialog):
    """Диалог редактирования с AI-инпейнтом."""
    modelLoaded = pyqtSignal(object, object)   # (inpainter|None, err|None)
    inpaintDone = pyqtSignal(object, object)   # (result_rgb|None, err|None)

    def __init__(self, image: QImage, ai_device: str, parent=None):
        self._ai_device = ai_device
        self._inpainter = None
        self._loading = False
        super().__init__(image, parent)
        self.setWindowTitle("Редактор области (AI инпейнт)")
        self.set_status("⏳ Загрузка модели...")
        self.btn_process.setEnabled(False)

        self.modelLoaded.connect(self._on_model_loaded, Qt.ConnectionType.QueuedConnection)
        self.inpaintDone.connect(self._on_inpaint_done, Qt.ConnectionType.QueuedConnection)

        self._load_inpainter_async()

    # ---- Настройки UI ----
    def info_text(self) -> str:
        return f"Модель: LaMa (InpainterV2, {self._ai_device})"

    def build_params_block(self):
        refine_group = QGroupBox("Параметры Refinement")
        refine_layout = QFormLayout()

        self.cb_refine = QCheckBox()
        self.cb_refine.setChecked(False)

        self.spin_n_iters = QSpinBox()
        self.spin_n_iters.setRange(5, 50)
        self.spin_n_iters.setValue(15)
        self.spin_n_iters.setEnabled(False)

        self.spin_max_scales = QSpinBox()
        self.spin_max_scales.setRange(1, 5)
        self.spin_max_scales.setValue(3)
        self.spin_max_scales.setEnabled(False)

        self.spin_px_budget = QSpinBox()
        self.spin_px_budget.setRange(500000, 4000000)
        self.spin_px_budget.setSingleStep(100000)
        self.spin_px_budget.setValue(1000000)
        self.spin_px_budget.setEnabled(False)

        def toggle_refine_params(checked):
            self.spin_n_iters.setEnabled(checked)
            self.spin_max_scales.setEnabled(checked)
            self.spin_px_budget.setEnabled(checked)

        self.cb_refine.toggled.connect(toggle_refine_params)

        refine_layout.addRow("Включить Refine:", self.cb_refine)
        refine_layout.addRow("Итерации (n_iters):", self.spin_n_iters)
        refine_layout.addRow("Масштабы (max_scales):", self.spin_max_scales)
        refine_layout.addRow("Лимит пикселей:", self.spin_px_budget)
        refine_group.setLayout(refine_layout)
        return refine_group

    # ---- Работа модели ----
    def _load_inpainter_async(self):
        if self._loading or self._inpainter is not None:
            return
        self._loading = True
        self.set_status("⏳ Загрузка модели...")

        def worker():
            err = None
            model = None
            try:
                model = _get_inpainter(self._ai_device)
            except Exception as e:
                traceback.print_exc()
                err = e
            self.modelLoaded.emit(model, err)

        threading.Thread(target=worker, daemon=True).start()

    def _on_model_loaded(self, model, err):
        self._loading = False
        if err:
            self._inpainter = None
            self.btn_process.setEnabled(False)
            self.set_status(f"❌ Ошибка загрузки модели: {err}")
            msg = QMessageBox(self)
            msg.setIcon(QMessageBox.Icon.Critical)
            msg.setWindowTitle("Ошибка LaMa")
            msg.setText(str(err))
            msg.setStandardButtons(QMessageBox.StandardButton.Ok)
            msg.exec()
        else:
            self._inpainter = model
            self.set_status("Модель загружена. Нарисуйте маску и нажмите «Обработать».")
            self.btn_process.setEnabled(True)

    def run(self, base_rgb: np.ndarray, mask_a: np.ndarray):
        if self._inpainter is None:
            self.set_status("⏳ Модель ещё загружается…")
            return None

        use_refine = self.cb_refine.isChecked()
        n_iters = self.spin_n_iters.value()
        max_scales = self.spin_max_scales.value()
        px_budget = self.spin_px_budget.value()

        try:
            self._inpainter.set_refine(
                use_refine,
                n_iters=n_iters,
                max_scales=max_scales,
                px_budget=px_budget
            )
        except Exception as e:
            self.finish_processing(None, e)
            return None

        def worker():
            err = None
            result_rgb = None
            try:
                result_rgb = self._inpainter(base_rgb, mask_a)
            except Exception as e:  # noqa: BLE001
                traceback.print_exc()
                err = e
            self.inpaintDone.emit(result_rgb, err)

        threading.Thread(target=worker, daemon=True).start()
        return None

    def _on_inpaint_done(self, result_rgb, err):
        self.finish_processing(result_rgb, err)


# ---------------------- Сам инструмент ----------------------
class RegionEditAItool(RegionEditTool):
    """
    Инструмент редактирования области с AI инпейнтом (InpainterV2).

    Использование:
      • Shift+ЛКМ — прямоугольник на картинке (как в скелете).
      • Откроется диалог, где рисуем маску и жмём «Обработать».
      • Настраиваем параметры Refinement при необходимости.
      • «Применить» — вставит результат точно в выбранную область.
    """
    tool_id = "region_edit_ai"
    title   = "AI удаление (Lama)"

    def create_editor_dialog(self, image: QImage, parent=None) -> RegionEditorDialog:
        # Возвращаем наш диалог вместо простого рисования
        return InpaintEditorDialog(image, ai_device=self.get_ai_device_str(), parent=parent)

    def build_ui(self, parent_layout) -> None:
        """Добавляем UI элементы в динамическую панель инструмента"""
        # Базовая подсказка
        hint = QLabel("Выделение: Shift + ЛКМ (прямоугольник)")
        hint.setStyleSheet("color: #666;")
        parent_layout.addWidget(hint)

        # Кнопка выгрузки AI
        btn_unload = QPushButton("🧹 Выгрузить AI")
        btn_unload.setToolTip("Освободить GPU память, выгрузив модель LaMa")
        btn_unload.clicked.connect(self._on_unload_ai)
        parent_layout.addWidget(btn_unload)

        # Информация о памяти (если модель загружена)
        self.memory_label = QLabel("")
        self.memory_label.setStyleSheet("color: #888; font-size: 10px;")
        parent_layout.addWidget(self.memory_label)

        # Обновить информацию о памяти
        self._update_memory_info()

    def _on_unload_ai(self):
        """Выгрузить AI модель из памяти"""
        if _unload_inpainter():
            if hasattr(self, 'memory_label'):
                self.memory_label.setText("✅ Модель выгружена, память освобождена")
            print("✅ LaMa инпейнтер выгружен из памяти")
        else:
            if hasattr(self, 'memory_label'):
                self.memory_label.setText("⚠️ Модель не была загружена")
            print("⚠️ Инпейнтер не был загружен или уже выгружен")

    def _update_memory_info(self):
        """Обновить информацию о памяти GPU"""
        if hasattr(self, 'memory_label'):
            try:
                global _INPAINTER
                if _INPAINTER is not None:
                    stats = _INPAINTER.get_memory_stats()
                    if stats.get('model_loaded', False):
                        allocated = stats.get('allocated_mb', 0)
                        self.memory_label.setText(f"💾 GPU: {allocated:.0f} MB")
                    else:
                        self.memory_label.setText("Модель не загружена")
                else:
                    self.memory_label.setText("Модель не загружена")
            except Exception:
                self.memory_label.setText("")
