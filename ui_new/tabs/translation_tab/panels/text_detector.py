from __future__ import annotations

import traceback
from typing import Dict, Iterable, List, TYPE_CHECKING

import os
import cv2
import numpy as np
from PyQt6.QtCore import QThread, QTimer, pyqtSignal
from PyQt6.QtGui import QImage, QPixmap
from PyQt6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDoubleSpinBox,
    QFrame,
    QHBoxLayout,
    QLabel,
    QMessageBox,
    QPushButton,
    QSpinBox,
    QVBoxLayout,
)

if TYPE_CHECKING:
    from ..textdetector.detector_ctd import ComicTextDetector
from ..utils import ImageLike
from config import UserConfig
from modules.utils_qt import qimage_to_numpy_bgr
from config import TEXT_DETECTOR_DIR
def _qimage_to_bgr(qimg: QImage) -> np.ndarray:
    return qimage_to_numpy_bgr(qimg)


def _load_bgr(img: ImageLike) -> np.ndarray:
    if isinstance(img, str):
        mat = cv2.imread(img, cv2.IMREAD_COLOR)
        if mat is None:
            raise FileNotFoundError(f"Не удалось открыть изображение: {img}")
        return mat
    if isinstance(img, QPixmap):
        return _load_bgr(img.toImage())
    if isinstance(img, QImage):
        return _qimage_to_bgr(img)
    raise TypeError(f"Неизвестный формат изображения: {type(img)}")


class _DetectionWorker(QThread):
    progress = pyqtSignal(int, int)
    finished = pyqtSignal(dict)
    failed = pyqtSignal(str)

    def __init__(self, images: List[ImageLike], indices: Iterable[int], detector):
        super().__init__()
        self.images = images
        self.indices = list(indices)
        self.detector = detector

    def run(self):
        try:
            results: Dict[int, dict] = {}
            total = len(self.indices)
            for pos, idx in enumerate(self.indices, start=1):
                if not (0 <= idx < len(self.images)):
                    self.progress.emit(pos, total)
                    continue
                img_bgr = _load_bgr(self.images[idx])
                mask, blk_list = self.detector.detect(img_bgr, None)
                if isinstance(mask, list) and not isinstance(blk_list, list):
                    mask, blk_list = blk_list, mask
                if mask is None or isinstance(mask, list):
                    mask = np.zeros(img_bgr.shape[:2], dtype=np.uint8)
                if blk_list is None:
                    blk_list = []
                results[idx] = {
                    "mask": mask,
                    "blocks": blk_list,
                    "size": (img_bgr.shape[1], img_bgr.shape[0]),
                }
                self.progress.emit(pos, total)
            self.finished.emit(results)
        except Exception as exc:  # pylint: disable=broad-except
            tb = traceback.format_exc()
            self.failed.emit(f"{exc}\n{tb}")


class _DetectedBlocksOcrWorker(QThread):
    progress = pyqtSignal(int, int)
    recognized = pyqtSignal(int, str)
    finished = pyqtSignal(int)
    failed = pyqtSignal(str)

    def __init__(self, canvas, tasks: List[dict]):
        super().__init__()
        self.canvas = canvas
        self.tasks = tasks

    def run(self):
        try:
            total = len(self.tasks)
            processed = 0
            for task_id, task in enumerate(self.tasks):
                crop = task.get("crop")
                text = self.canvas._perform_ocr(crop) if crop is not None else ""
                if text:
                    self.recognized.emit(int(task_id), str(text))
                processed += 1
                self.progress.emit(processed, total)
            self.finished.emit(processed)
        except Exception as exc:  # pylint: disable=broad-except
            tb = traceback.format_exc()
            self.failed.emit(f"{exc}\n{tb}")


class TextDetectorPanel(QFrame):
    """
    Простая панель управления детектором текста для вкладки перевода.

    Содержит кнопки детекта текста (текущая страница / все) и автозапуска OCR по найденным блокам.
    """

    def __init__(self, parent, canvas):
        super().__init__(parent)
        self.canvas = canvas
        self._detector: ComicTextDetector | None = None
        self._worker: _DetectionWorker | None = None
        self._replace_on_finish: bool = True
        self._pending_detector_params: dict[str, object] = {}
        self._ocr_running: bool = False
        self._ocr_worker: _DetectedBlocksOcrWorker | None = None
        self._ocr_tasks: List[dict] = []
        self._ocr_recognized_count: int = 0
        self._loading_config: bool = False
        self._forced_device: str = self._detector_device_from_ai()

        self.setFrameShape(QFrame.Shape.StyledPanel)
        self.setStyleSheet("""
            QFrame { background: #202020; border: 1px solid #444; color: #ddd; }
            QLabel { color: #ddd; }
            QPushButton { background: #2b2b2b; color: #eee; border: 1px solid #555; padding: 6px 10px; }
            QPushButton:hover { background: #333; }
        """)

        root = QVBoxLayout(self)
        hdr = QHBoxLayout()
        lbl_title = QLabel("Детектор текста")
        lbl_title.setStyleSheet("font-weight:700;color:#fff;")
        btn_close = QPushButton("✕")
        btn_close.setFixedWidth(28)
        btn_close.clicked.connect(self.hide)
        hdr.addWidget(lbl_title)
        hdr.addStretch(1)
        hdr.addWidget(btn_close)
        root.addLayout(hdr)

        self.lbl_status = QLabel("Готов к работе")
        self.lbl_progress = QLabel("")
        root.addWidget(self.lbl_status)
        root.addWidget(self.lbl_progress)

        opts = QFrame()
        opts.setFrameShape(QFrame.Shape.StyledPanel)
        opts_lay = QVBoxLayout(opts)
        opts_lay.setContentsMargins(6, 6, 6, 6)
        opts_lay.setSpacing(6)
        self.chk_draw_lines = QCheckBox("Обводить строки")
        self.chk_draw_lines.setChecked(True)
        self.chk_draw_mask = QCheckBox("Рисовать маску")
        self.chk_draw_mask.setChecked(True)
        self.spin_block_expand = QSpinBox()
        self.spin_block_expand.setRange(0, 50)
        self.spin_block_expand.setValue(0)
        self.chk_merge_close = QCheckBox("Объединять ближайшие блоки")
        self.chk_merge_close.setChecked(False)
        self.spin_merge_gap = QSpinBox()
        self.spin_merge_gap.setRange(0, 50)
        self.spin_merge_gap.setValue(5)
        self.spin_merge_gap.setEnabled(False)

        def _row(lbl_text: str, widget):
            row = QHBoxLayout()
            lbl = QLabel(lbl_text)
            row.addWidget(lbl)
            row.addWidget(widget)
            row.addStretch(1)
            opts_lay.addLayout(row)

        opts_lay.addWidget(self.chk_draw_lines)
        opts_lay.addWidget(self.chk_draw_mask)
        _row("Расширение блока:", self.spin_block_expand)
        opts_lay.addWidget(self.chk_merge_close)
        _row("Дистанция объединения:", self.spin_merge_gap)
        root.addWidget(opts)

        det_opts = self._build_detector_params_ui()
        root.addWidget(det_opts)

        btn_current = QPushButton("Найти текст на текущей странице")
        btn_all = QPushButton("Найти текст везде")
        self.btn_ocr_current = QPushButton("Распознать на текущей странице")
        self.btn_ocr_all = QPushButton("Распознать все")
        btn_current.clicked.connect(self._on_detect_current)
        btn_all.clicked.connect(self._on_detect_all)
        self.btn_ocr_current.clicked.connect(self._on_ocr_current)
        self.btn_ocr_all.clicked.connect(self._on_ocr_all)
        root.addWidget(btn_current)
        root.addWidget(btn_all)
        root.addWidget(self.btn_ocr_current)
        root.addWidget(self.btn_ocr_all)
        root.addStretch(1)

        # опции отрисовки
        self.chk_draw_lines.toggled.connect(self._on_opts_changed)
        self.chk_draw_mask.toggled.connect(self._on_opts_changed)
        self.spin_block_expand.valueChanged.connect(self._on_opts_changed)
        self.chk_merge_close.toggled.connect(self._on_merge_toggle)
        self.spin_merge_gap.valueChanged.connect(self._on_opts_changed)
        self._load_config_from_user()
        self._apply_canvas_options()
        self._update_ocr_buttons_state()

        self.hide()

        self._ocr_poll = QTimer(self)
        self._ocr_poll.setInterval(1000)
        self._ocr_poll.timeout.connect(self._update_ocr_buttons_state)
        self._ocr_poll.start()

    def _detector_device_from_ai(self) -> str:
        dev = getattr(self.canvas, "ai_device", None)
        if dev is None:
            return "cpu"
        dev_s = str(dev).strip().lower()
        return "cuda" if dev_s == "cuda" or dev_s.startswith("cuda:") else "cpu"

    def _build_detector_params_ui(self) -> QFrame:
        frame = QFrame()
        frame.setFrameShape(QFrame.Shape.StyledPanel)
        lay = QVBoxLayout(frame)
        lay.setContentsMargins(6, 6, 6, 6)
        lay.setSpacing(6)

        lbl = QLabel("Параметры ComicTextDetector")
        lbl.setStyleSheet("font-weight:600;")
        lay.addWidget(lbl)

        def add_row(text: str, widget):
            row = QHBoxLayout()
            row.addWidget(QLabel(text))
            row.addWidget(widget)
            row.addStretch(1)
            lay.addLayout(row)

        # device
        self.cmb_device = QComboBox()
        self._set_combo_items(self.cmb_device, [self._forced_device], self._forced_device)
        self.cmb_device.setEnabled(False)
        self.cmb_device.setToolTip("Управляется глобальной настройкой ИИ-устройства.")
        self._pending_detector_params["device"] = self._forced_device

        # detect size
        self.cmb_detect_size = QComboBox()
        detect_sizes = [896, 1024, 1152, 1280]
        self._set_combo_items(self.cmb_detect_size, [str(v) for v in detect_sizes], "1280")
        self.cmb_detect_size.currentTextChanged.connect(lambda val: self._on_param_changed("detect_size", int(val)))
        self._pending_detector_params["detect_size"] = 1280

        # rearrange batches
        self.cmb_rearrange = QComboBox()
        rearrange_opts = [1, 2, 4, 6, 8, 12, 16, 24, 32]
        self._set_combo_items(self.cmb_rearrange, [str(v) for v in rearrange_opts], "4")
        self.cmb_rearrange.currentTextChanged.connect(lambda val: self._on_param_changed("det_rearrange_max_batches", int(val)))
        self._pending_detector_params["det_rearrange_max_batches"] = 4

        # font size multiplier
        self.spin_font_mul = QDoubleSpinBox()
        self.spin_font_mul.setDecimals(2)
        self.spin_font_mul.setRange(0.1, 8.0)
        self.spin_font_mul.setSingleStep(0.1)
        self.spin_font_mul.setValue(1.0)
        self.spin_font_mul.valueChanged.connect(lambda val: self._on_param_changed("font size multiplier", float(val)))
        self._pending_detector_params["font size multiplier"] = 1.0

        # font max / min (-1 disables)
        self.spin_font_max = QDoubleSpinBox()
        self.spin_font_max.setDecimals(1)
        self.spin_font_max.setRange(-1.0, 500.0)
        self.spin_font_max.setSingleStep(1.0)
        self.spin_font_max.setValue(-1.0)
        self.spin_font_max.setSpecialValueText("выкл (-1)")
        self.spin_font_max.valueChanged.connect(lambda val: self._on_param_changed("font size max", float(val)))
        self._pending_detector_params["font size max"] = -1.0

        self.spin_font_min = QDoubleSpinBox()
        self.spin_font_min.setDecimals(1)
        self.spin_font_min.setRange(-1.0, 500.0)
        self.spin_font_min.setSingleStep(1.0)
        self.spin_font_min.setValue(-1.0)
        self.spin_font_min.setSpecialValueText("выкл (-1)")
        self.spin_font_min.valueChanged.connect(lambda val: self._on_param_changed("font size min", float(val)))
        self._pending_detector_params["font size min"] = -1.0

        # detector mask dilation (separate от отрисовки)
        self.spin_det_mask = QSpinBox()
        self.spin_det_mask.setRange(0, 30)
        self.spin_det_mask.setValue(2)
        self.spin_det_mask.valueChanged.connect(lambda val: self._on_param_changed("mask dilate size", int(val)))
        self._pending_detector_params["mask dilate size"] = 2

        add_row("Устройство:", self.cmb_device)
        add_row("Размер детекции:", self.cmb_detect_size)
        add_row("Макс. батчи rearrange:", self.cmb_rearrange)
        add_row("Множитель шрифта:", self.spin_font_mul)
        add_row("Макс. шрифт:", self.spin_font_max)
        add_row("Мин. шрифт:", self.spin_font_min)
        add_row("Расширение маски:", self.spin_det_mask)

        return frame

    # --- config helpers ---
    def _config_section(self):
        try:
            return UserConfig.TranslarionTab.TextDetector
        except Exception:
            return None

    def _coerce_int(self, value, default: int) -> int:
        try:
            return int(value)
        except Exception:
            return default

    def _coerce_float(self, value, default: float) -> float:
        try:
            return float(value)
        except Exception:
            return default

    def _set_checked(self, checkbox: QCheckBox, value: bool):
        checkbox.blockSignals(True)
        checkbox.setChecked(bool(value))
        checkbox.blockSignals(False)

    def _set_spin_value(self, spin: QSpinBox, value):
        spin.blockSignals(True)
        try:
            spin.setValue(int(value))
        except Exception:
            pass
        spin.blockSignals(False)

    def _set_dspin_value(self, spin: QDoubleSpinBox, value):
        spin.blockSignals(True)
        try:
            spin.setValue(float(value))
        except Exception:
            pass
        spin.blockSignals(False)

    def _set_combo_value(self, combo: QComboBox, value):
        combo.blockSignals(True)
        try:
            idx = combo.findText(str(value))
            if idx < 0:
                combo.addItem(str(value))
                idx = combo.findText(str(value))
            if idx >= 0:
                combo.setCurrentIndex(idx)
        finally:
            combo.blockSignals(False)

    def _load_config_from_user(self):
        cfg = self._config_section()
        if not cfg:
            return
        self._loading_config = True
        try:
            self._forced_device = self._detector_device_from_ai()
            self._set_checked(self.chk_draw_lines, getattr(cfg, "draw_lines", True))
            self._set_checked(self.chk_draw_mask, getattr(cfg, "draw_mask", True))
            self._set_spin_value(self.spin_block_expand, getattr(cfg, "block_expand_px", 0))
            merge_close = bool(getattr(cfg, "merge_close", False))
            self._set_checked(self.chk_merge_close, merge_close)
            self.spin_merge_gap.setEnabled(merge_close)
            self._set_spin_value(self.spin_merge_gap, getattr(cfg, "merge_gap_px", 5))

            params_cfg = getattr(cfg, "params", None)
            if params_cfg:
                dev = self._forced_device
                detect_size = self._coerce_int(getattr(params_cfg, "detect_size", self._pending_detector_params.get("detect_size", 1280)), 1280)
                rearrange = self._coerce_int(getattr(params_cfg, "det_rearrange_max_batches", self._pending_detector_params.get("det_rearrange_max_batches", 4)), 4)
                font_mul = self._coerce_float(getattr(params_cfg, "font size multiplier", self._pending_detector_params.get("font size multiplier", 1.0)), 1.0)
                font_max = self._coerce_float(getattr(params_cfg, "font size max", self._pending_detector_params.get("font size max", -1.0)), -1.0)
                font_min = self._coerce_float(getattr(params_cfg, "font size min", self._pending_detector_params.get("font size min", -1.0)), -1.0)
                mask_dilate = self._coerce_int(getattr(params_cfg, "mask dilate size", self._pending_detector_params.get("mask dilate size", 2)), 2)

                self._pending_detector_params.update({
                    "device": dev,
                    "detect_size": detect_size,
                    "det_rearrange_max_batches": rearrange,
                    "font size multiplier": font_mul,
                    "font size max": font_max,
                    "font size min": font_min,
                    "mask dilate size": mask_dilate,
                })

                self._set_combo_value(self.cmb_device, dev)
                self._set_combo_items(self.cmb_device, [dev], dev)
                self._set_combo_value(self.cmb_detect_size, detect_size)
                self._set_combo_value(self.cmb_rearrange, rearrange)
                self._set_dspin_value(self.spin_font_mul, font_mul)
                self._set_dspin_value(self.spin_font_max, font_max)
                self._set_dspin_value(self.spin_font_min, font_min)
                self._set_spin_value(self.spin_det_mask, mask_dilate)
        finally:
            self._loading_config = False

    def _save_options_to_config(self):
        if self._loading_config:
            return
        cfg = self._config_section()
        if not cfg:
            return
        try:
            cfg.draw_lines = bool(self.chk_draw_lines.isChecked())
            cfg.draw_mask = bool(self.chk_draw_mask.isChecked())
            cfg.block_expand_px = int(self.spin_block_expand.value())
            cfg.merge_close = bool(self.chk_merge_close.isChecked())
            cfg.merge_gap_px = int(self.spin_merge_gap.value())
            UserConfig.save()
        except Exception:
            traceback.print_exc()

    def _save_detector_params_to_config(self):
        if self._loading_config:
            return
        cfg = self._config_section()
        if not cfg:
            return
        try:
            params_cfg = getattr(cfg, "params", None)
            if params_cfg is None:
                cfg.params = {}
                params_cfg = cfg.params
            for key, val in self._pending_detector_params.items():
                try:
                    setattr(params_cfg, key, val)
                except Exception:
                    pass
            UserConfig.save()
        except Exception:
            traceback.print_exc()

    # --- actions ---
    def _ensure_detector(self) -> ComicTextDetector | None:
        if self._detector is not None:
            return self._detector
        try:
            if os.path.exists(os.path.join(TEXT_DETECTOR_DIR, "comictextdetector.pt")):
                from ..textdetector.detector_ctd import ComicTextDetector  # lazy import, тяжёлые зависимости
                self._detector = ComicTextDetector()
                self._pending_detector_params["device"] = self._detector_device_from_ai()
                # применяем накопленные параметры, если пользователь менял их до инициализации детектора
                for key, val in self._pending_detector_params.items():
                    try:
                        self._detector.updateParam(key, val)
                    except Exception:
                        traceback.print_exc()
                self._sync_detector_widgets()
                return self._detector
            else:
                QMessageBox.critical("Ошибка", "Загрузите модель детектора текста в менеджере моделей ИИ.")
                return None
        except Exception:
            traceback.print_exc()
            return None

    def _on_detect_current(self):
        idx = self.canvas.current_page_index()
        self._start_detection([idx], replace=False)

    def _on_detect_all(self):
        self._start_detection(range(len(getattr(self.canvas, "images", []))), replace=True)

    def _start_detection(self, indices: Iterable[int], *, replace: bool):
        if self._worker:
            return  # уже в процессе
        idx_list = list(indices)
        if not idx_list:
            self._set_status("Нет доступных страниц", error=True)
            return
        detector = self._ensure_detector()
        if detector is None:
            self._set_status("Не удалось инициализировать детектор", error=True)
            QMessageBox.warning(self, "Детектор текста", "Не удалось загрузить ComicTextDetector. Проверьте зависимости и модели.")
            return
        self._replace_on_finish = replace
        self._set_status("Поиск текста...", running=True)
        self._worker = _DetectionWorker(self.canvas.images, idx_list, detector)
        self._worker.progress.connect(self._on_progress)
        self._worker.finished.connect(self._on_finished)
        self._worker.failed.connect(self._on_failed)
        self._worker.finished.connect(self._cleanup_worker)
        self._worker.failed.connect(self._cleanup_worker)
        self._worker.start()
        self._update_ocr_buttons_state()

    def _cleanup_worker(self, *args):
        if self._worker:
            self._worker.deleteLater()
        self._worker = None
        self._update_ocr_buttons_state()

    def _on_progress(self, current: int, total: int):
        self.lbl_progress.setText(f"{current} / {total}")

    def _on_finished(self, results: Dict[int, dict]):
        total_blocks = sum(len(v.get("blocks", [])) for v in results.values())
        self.canvas.set_text_detection_results(results, replace=self._replace_on_finish)
        self._set_status(f"Готово. Найдено блоков: {total_blocks}", error=False)
        self.lbl_progress.setText("")
        self._update_ocr_buttons_state()

    def _on_failed(self, message: str):
        self._set_status("Ошибка поиска текста", error=True)
        self.lbl_progress.setText("")
        QMessageBox.critical(self, "Детектор текста", message)
        self._update_ocr_buttons_state()

    def _set_status(self, text: str, error: bool = False, running: bool = False):
        color = "#f66" if error else ("#f7c948" if running else "#8fda8f")
        self.lbl_status.setStyleSheet(f"color:{color};font-weight:600;")
        self.lbl_status.setText(text)
        self._update_ocr_buttons_state()

    def _on_opts_changed(self, *args):
        self._apply_canvas_options()
        self._save_options_to_config()

    def _on_merge_toggle(self, checked: bool):
        self.spin_merge_gap.setEnabled(checked)
        self._apply_canvas_options()
        self._save_options_to_config()

    def _apply_canvas_options(self):
        self.canvas.set_textdetector_options(
            draw_lines=self.chk_draw_lines.isChecked(),
            draw_mask=self.chk_draw_mask.isChecked(),
            block_expand_px=self.spin_block_expand.value(),
            merge_gap_px=self.spin_merge_gap.value() if self.chk_merge_close.isChecked() else 0,
        )

    # --- detector param helpers ---
    def _on_param_changed(self, key: str, value):
        if key == "device":
            value = self._detector_device_from_ai()
        # запоминаем выбор и, если детектор уже есть, передаем ему сразу
        self._pending_detector_params[key] = value
        if self._detector is None:
            self._save_detector_params_to_config()
            return
        try:
            self._detector.updateParam(key, value)
        except Exception:
            traceback.print_exc()
        self._save_detector_params_to_config()

    def _set_combo_items(self, combo: QComboBox, options, current=None):
        combo.blockSignals(True)
        combo.clear()
        for opt in options:
            combo.addItem(str(opt))
        if current is not None:
            idx = combo.findText(str(current))
            if idx >= 0:
                combo.setCurrentIndex(idx)
        combo.blockSignals(False)

    def _sync_detector_widgets(self):
        """Обновить значения и списки опций виджетов из текущего детектора."""
        det = self._detector
        if det is None:
            return
        try:
            self._forced_device = self._detector_device_from_ai()
            params = getattr(det, "params", {}) or {}
            dev_cfg = params.get("device")
            if isinstance(dev_cfg, dict):
                opts = dev_cfg.get("options") or []
                cur = self._forced_device
                if opts:
                    forced_opts = [opt for opt in opts if str(opt).lower().startswith("cuda")] if cur == "cuda" else [opt for opt in opts if str(opt).lower() == "cpu"]
                    if not forced_opts:
                        forced_opts = [cur]
                    self._set_combo_items(self.cmb_device, forced_opts, cur)
        except Exception:
            traceback.print_exc()

        def set_combo(combo: QComboBox, val):
            combo.blockSignals(True)
            idx = combo.findText(str(val))
            if idx < 0:
                combo.addItem(str(val))
                idx = combo.findText(str(val))
            combo.setCurrentIndex(idx)
            combo.blockSignals(False)

        def set_dspin(spin: QDoubleSpinBox, val):
            spin.blockSignals(True)
            try:
                spin.setValue(float(val))
            except Exception:
                pass
            spin.blockSignals(False)

        def set_spin(spin: QSpinBox, val):
            spin.blockSignals(True)
            try:
                spin.setValue(int(val))
            except Exception:
                pass
            spin.blockSignals(False)

        try:
            set_combo(self.cmb_detect_size, det.get_param_value("detect_size"))
            set_combo(self.cmb_rearrange, det.get_param_value("det_rearrange_max_batches"))
            set_combo(self.cmb_device, self._forced_device)
            set_dspin(self.spin_font_mul, det.get_param_value("font size multiplier"))
            set_dspin(self.spin_font_max, det.get_param_value("font size max"))
            set_dspin(self.spin_font_min, det.get_param_value("font size min"))
            set_spin(self.spin_det_mask, det.get_param_value("mask dilate size"))
        except Exception:
            traceback.print_exc()

    # --- OCR helpers ---
    def _has_text_detections(self, page_idx: int | None = None) -> bool:
        try:
            results = getattr(self.canvas, "_textdetector_results", {}) or {}
        except Exception:
            traceback.print_exc()
            results = {}
        if not isinstance(results, dict) or not results:
            return False
        if page_idx is None:
            for idx in results.keys():
                try:
                    if self.canvas.get_detected_block_rects(int(idx)):
                        return True
                except Exception:
                    continue
            return False
        try:
            rects = self.canvas.get_detected_block_rects(int(page_idx))
            return bool(rects)
        except Exception:
            traceback.print_exc()
            return False

    def _update_ocr_buttons_state(self):
        ocr_ready = getattr(self.canvas, "is_ocr_ready", lambda: False)()
        busy = self._worker is not None or self._ocr_worker is not None or self._ocr_running
        any_detections = self._has_text_detections()
        current_has = self._has_text_detections(self.canvas.current_page_index()) if hasattr(self.canvas, "current_page_index") else False
        can_run = ocr_ready and any_detections and not busy
        self.btn_ocr_all.setEnabled(bool(can_run))
        self.btn_ocr_current.setEnabled(bool(can_run and current_has))

    def _on_ocr_current(self):
        idx = self.canvas.current_page_index()
        self._run_ocr_for_indices([idx])

    def _on_ocr_all(self):
        indices = []
        try:
            for key in getattr(self.canvas, "_textdetector_results", {}).keys():
                try:
                    indices.append(int(key))
                except Exception:
                    continue
        except Exception:
            traceback.print_exc()
        if not indices:
            indices = list(range(len(getattr(self.canvas, "images", []))))
        self._run_ocr_for_indices(indices)

    def _run_ocr_for_indices(self, indices: Iterable[int]):
        if self._ocr_running or self._worker or self._ocr_worker:
            return
        if not getattr(self.canvas, "is_ocr_ready", lambda: False)():
            self._set_status("OCR не загружен", error=True)
            QMessageBox.warning(self, "OCR", "Загрузите OCR в панели настроек и попробуйте снова.")
            return
        idx_list = [int(i) for i in indices if self._has_text_detections(i)]
        if not idx_list:
            self._set_status("Нет результатов детекта", error=True)
            return

        self._ocr_running = True
        self._update_ocr_buttons_state()
        tasks = self.canvas.collect_ocr_tasks_for_detected_blocks(idx_list)
        if not tasks:
            self._ocr_running = False
            self._set_status("Нет блоков для OCR", error=True)
            self.lbl_progress.setText("")
            self._update_ocr_buttons_state()
            return
        self._ocr_tasks = tasks
        self._ocr_recognized_count = 0
        self.lbl_progress.setText(f"0 / {len(tasks)}")
        self._set_status("Распознавание...", running=True)
        self._ocr_worker = _DetectedBlocksOcrWorker(self.canvas, tasks)
        self._ocr_worker.progress.connect(self._on_ocr_progress)
        self._ocr_worker.recognized.connect(self._on_ocr_recognized)
        self._ocr_worker.finished.connect(self._on_ocr_finished)
        self._ocr_worker.failed.connect(self._on_ocr_failed)
        self._ocr_worker.finished.connect(self._cleanup_ocr_worker)
        self._ocr_worker.failed.connect(self._cleanup_ocr_worker)
        self._ocr_worker.start()

    def _on_ocr_recognized(self, task_id: int, text: str):
        if not (0 <= task_id < len(self._ocr_tasks)):
            return
        task = self._ocr_tasks[task_id]
        crop_scene = task.get("crop_scene")
        if crop_scene is None:
            return
        try:
            self.canvas._apply_ocr_text_to_scene_rect(
                int(task.get("target_idx", -1)),
                crop_scene,
                str(text),
            )
            self._ocr_recognized_count += 1
        except Exception:
            traceback.print_exc()

    def _on_ocr_finished(self, processed: int):
        self._ocr_running = False
        self.lbl_progress.setText("")
        if self._ocr_recognized_count > 0:
            self._set_status(f"OCR готово. Блоков: {self._ocr_recognized_count}", error=False)
        elif processed > 0:
            self._set_status("Распознавание завершено, текст не найден", error=False)
        else:
            self._set_status("Нет блоков для OCR", error=True)
        self._update_ocr_buttons_state()

    def _on_ocr_failed(self, message: str):
        self._ocr_running = False
        self._set_status("Ошибка OCR", error=True)
        self.lbl_progress.setText("")
        QMessageBox.critical(self, "OCR", message)
        self._update_ocr_buttons_state()

    def _cleanup_ocr_worker(self, *args):
        worker = self._ocr_worker
        self._ocr_worker = None
        if worker:
            worker.deleteLater()
        self._ocr_tasks = []
        self._update_ocr_buttons_state()

    def _on_ocr_progress(self, current: int, total: int):
        if total > 0:
            self.lbl_progress.setText(f"{current} / {total}")
